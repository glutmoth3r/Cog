use cog::backend::CompiledProgram;
use cog::runtime::{RuntimeContext, VmCapabilities};
use cog::sdk::{Engine, RunContext};
use cog::{bytecode, interpreter, project};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const PRELUDE: &str = "@!include(\"std\") use err::Error;\n\
    fn require(condition: bool) { if !condition { crash(\"assertion failed\"); } }\n";

fn compile(body: &str) -> CompiledProgram {
    let source = format!("{PRELUDE}{body}");
    let program = project::compile_source("stdlib-batch.cog", &source)
        .unwrap_or_else(|error| panic!("{error}\n{source}"));
    bytecode::decode(&bytecode::encode(&program).unwrap()).unwrap()
}

/// Execute decoded bytecode on the VM and again with the JIT requested.
fn run(body: &str) {
    let source = format!("{PRELUDE}{body}");
    let engine = Engine::new();
    let program = engine
        .decode(&bytecode::encode(&compile(body)).unwrap())
        .unwrap();
    for jit in [false, true, true] {
        let mut output = Vec::new();
        program
            .run_in(&mut RunContext::new(&mut output).with_jit(jit))
            .unwrap_or_else(|error| panic!("JIT={jit}: {error}\n{source}"));
    }
}

fn run_limited(body: &str, capabilities: VmCapabilities, limit: Option<usize>) {
    let program = compile(body);
    let mut sink = Vec::new();
    let mut context = RuntimeContext::new(&mut sink, capabilities);
    context.limits.max_buffer_bytes = limit;
    interpreter::run_with_context(&program, &mut context)
        .unwrap_or_else(|error| panic!("{error}\n{body}"));
}

fn rejects(body: &str, expected: &str) {
    let source = format!("{PRELUDE}{body}");
    let error = match project::compile_source("rejected.cog", &source) {
        Ok(_) => panic!("accepted:\n{source}"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains(expected), "{error}");
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "cog-std-batch-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().unwrap())
}

#[test]
fn precise_result_types_flow_into_typed_code() {
    run(r#"
        use str; use hex; use base64; use random; use bytes; use array;
        fn width(text: str) -> int { len(text) }
        fn size(data: bytes) -> int { bytes::len(data)!! }
        fn main() {
            const names: str[] = str::split("a,bb,é€", ",")!!;
            const widths: int[] = array::map(str::split("a,bb,é€", ",")!!, width)!!;
            require(widths[0]!! == 1 && widths[1]!! == 2 && widths[2]!! == 2);
            for part in str::split("x y", " ")!! { require(width(part) == 1); }
            require(size(hex::decode("00ff")!!) == 2);
            require(size(base64::decode("TWFu")!!) == 3);
            const sample: int = random::next()!!;
            const ranged: int = random::range(3, 5)!!;
            const flag: bool = random::boolean()!!;
            require(sample >= 0 && ranged >= 3 && ranged < 5 && (flag || !flag));
            when str::split("a", "") is Error::InvalidArgument {} else { crash("separator"); }
            when hex::decode("zz") is Error::ParseFailure {} else { crash("hex"); }
            when base64::decode("?") is Error::ParseFailure {} else { crash("base64"); }
            when random::range(5, 5) is Error::InvalidArgument {} else { crash("range"); }
        }"#);
    rejects(
        "use str; fn main() { var parts = str::split(\"a\", \",\")!!; parts.append(1)!!; }",
        "int",
    );
}

#[test]
fn slice_counts_unicode_scalars_and_does_not_clamp() {
    run(r#"
        use str;
        fn main() {
            const text = "aé€🦀z";
            require(str::slice(text, 0, 5) == text);
            require(str::slice(text, 1, 3) == "é€🦀");
            require(str::slice(text, 3, 2) == "🦀z");
            require(str::slice(text, 4, 1) == "z");
            require(str::slice(text, 5, 0) == "");
            require(str::slice("", 0, 0) == "");
            require(str::slice(count: 1, offset: 2, text: text) == "€");
            for bad in [(-1, 1), (0, -1), (6, 0), (5, 1), (1, 5)] {
                when str::slice(text, bad.0, bad.1) is Error::InvalidArgument(op) {
                    require(op == "str::slice");
                } else { crash("range accepted"); }
            }
            when str::slice(text, 1, 9223372036854775807) is Error::InvalidArgument {}
            else { crash("huge count"); }
            when str::slice((1 / 0) as str, 0, 0) is Error::DivisionByZero {} else { crash("text"); }
            when str::slice(text, 1 / 0, 0) is Error::DivisionByZero {} else { crash("offset"); }
            when str::slice(text, 0, 1 / 0) is Error::DivisionByZero {} else { crash("count"); }
        }"#);
}

#[test]
fn split_whitespace_matches_trim_and_drops_empty_fields() {
    let mut source = String::from(
        "use str; use array;\nfn main() {\n\
         require(str::split_whitespace(\"\")!!.len() == 0);\n\
         require(str::split_whitespace(\" \\t\\r\\n \")!!.len() == 0);\n\
         require(array::equals(str::split_whitespace(\"one\")!!, [\"one\"]));\n",
    );
    // Every White_Space scalar separates; look-alikes such as U+200B do not.
    for space in "\u{9}\u{a}\u{b}\u{c}\u{d} \u{85}\u{a0}\u{1680}\u{2000}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}".chars() {
        source.push_str(&format!(
            "require(array::equals(str::split_whitespace(\"{space}é{space}{space}€\u{200b}x{space}\")!!, [\"é\", \"€\u{200b}x\"]));\n"
        ));
    }
    source.push_str(
        "when str::split_whitespace((1 / 0) as str) is Error::DivisionByZero {} else { crash(\"lost\"); }\n}",
    );
    run(&source);
}

#[test]
fn existing_text_search_keeps_its_results_after_sharing_the_matcher() {
    run(r#"
        use str;
        fn main() {
            require(str::find("é-aaaa", "aa") == 2);
            require(str::rfind("é-aaaa", "aa") == 4);
            require(str::count("aaaaa", "aa") == 2);
            require(str::find("a", "ab") == nil);
            require(str::split_once("k=é=v", "=")!!.1 == "é=v");
            require(str::rsplit_once("k=é=v", "=")!!.0 == "k=é");
            require(str::starts_with("éa", "é") && str::ends_with("aé", "é"));
        }"#);
}

#[test]
fn byte_buffers_compare_and_search() {
    run(r#"
        use bytes;
        fn b(text: str) -> bytes { bytes::from_str(text)!! }
        fn main() {
            require(bytes::equals(b(""), b("")));
            require(bytes::equals(b("é"), b("é")));
            require(!bytes::equals(b("ab"), b("ac")));
            require(!bytes::equals(b("ab"), b("abc")));
            require(bytes::starts_with(b("GET /"), b("GET")));
            require(bytes::starts_with(b("a"), b("")));
            require(!bytes::starts_with(b("a"), b("ab")));
            require(!bytes::starts_with(b("ab"), b("b")));

            const data = b("abab\r\n\r\nab");
            require(bytes::index_of(data, 97) == 0);
            require(bytes::index_of(data, 97, 1) == 2);
            require(bytes::index_of(data, 97, start: 9) == nil);
            require(bytes::index_of(data, 97, 10) == nil);
            require(bytes::index_of(data, 0) == nil);
            require(bytes::index_of(b(""), 0) == nil);
            require(bytes::find(data, b("\r\n\r\n")) == 4);
            require(bytes::find(data, b("ab")) == 0);
            require(bytes::find(data, b("ab"), 1) == 2);
            require(bytes::find(data, b("ab"), start: 3) == 8);
            require(bytes::find(data, b("ab"), 9) == nil);
            require(bytes::find(data, b("zz")) == nil);
            require(bytes::find(data, b("")) == 0);
            require(bytes::find(data, b(""), 10) == 10);
            require(bytes::find(b(""), b("a")) == nil);
            require(bytes::find(b("aaab"), b("aab")) == 1);
            require(bytes::find(b("é€"), b("€")) == 2);

            for bad in [-1, 11] {
                when bytes::index_of(data, 97, bad) is Error::InvalidArgument(op) {
                    require(op == "bytes::index_of");
                } else { crash("start"); }
                when bytes::find(data, b("a"), bad) is Error::InvalidArgument(op) {
                    require(op == "bytes::find");
                } else { crash("start"); }
            }
            for bad in [-1, 256] {
                when bytes::index_of(data, bad) is Error::InvalidArgument {} else { crash("byte"); }
            }
            when bytes::index_of(data, 1 / 0) is Error::DivisionByZero {} else { crash("value"); }
            when bytes::find(data, bytes::new(-1)) is Error {} else { crash("pattern"); }
            when bytes::equals(data, bytes::new(-1)) is Error {} else { crash("operand"); }
        }"#);
}

#[test]
fn bytes_module_keeps_primitives_reachable_through_every_import_form() {
    run(r#"
        use bytes::{len as size, from_str, find};
        use bytes;
        struct Frame { payload: bytes }
        fn main() {
            const frame = Frame(from_str("abc")!!);
            require(size(frame.payload) == 3);
            require(find(frame.payload, from_str("c")!!) == 2);
            require(bytes::len(bytes::slice(frame.payload, 1, 2)!!) == 2);
        }"#);
    run(r#"
        use bytes::*;
        fn main() {
            require(len(from_str("ab")!!) == 2);
            require(equals(from_str("ab")!!, concat(from_str("a")!!, from_str("b")!!)!!));
        }"#);
}

#[test]
fn map_find_returns_nil_for_missing_keys() {
    run(r#"
        use collections::{Hashable, Map};
        struct Key { id: int }
        var hashes = 0;
        extend Key with Hashable {
            fn hash_code(self) -> int {
                hashes += 1;
                if self.id < 0 { return Error::InvalidArgument("key"); }
                self.id
            }
            fn equals(self, other: Key) -> bool { self.id == other.id }
        }
        fn main() {
            var ages = Map::new[str, int]();
            ages.set("Zoë", 7)!!;
            const found: int? = ages.find("Zoë")!!;
            require(found == 7);
            require(ages.find("nobody") == nil);
            require((ages.find("nobody") ?? 3) == 3);

            var optional: Map[str, int?] = Map::new();
            optional.set("empty", nil)!!;
            require(optional.find("empty") == nil && optional.contains("empty"));

            var keyed = Map::new[Key, str]();
            keyed.set(Key(1), "one")!!;
            hashes = 0;
            require(keyed.find(Key(1)) == "one");
            require(hashes == 1);
            when keyed.find(Key(-1)) is Error::InvalidArgument {} else { crash("hash Error"); }
        }"#);
}

#[test]
fn ascii_helpers_cover_hex_digits_and_case_insensitive_text() {
    run(r#"
        use ascii; use str; use hex;
        fn main() {
            require(ascii::hex_value(48) == 0 && ascii::hex_value(57) == 9);
            require(ascii::hex_value(97) == 10 && ascii::hex_value(102) == 15);
            require(ascii::hex_value(65) == 10 && ascii::hex_value(70) == 15);
            for byte in [0, 47, 58, 64, 71, 96, 103, 255] {
                require(ascii::hex_value(byte) == nil);
            }
            for byte in [-1, 256] {
                when ascii::hex_value(byte) is Error::InvalidArgument(op) {
                    require(op == "ascii::hex_value");
                } else { crash("range"); }
            }
            require(hex::encode(hex::decode("aF09")!!) == "af09");

            require(str::equals_ignore_ascii_case("", ""));
            require(str::equals_ignore_ascii_case("Content-Type", "content-TYPE"));
            require(str::equals_ignore_ascii_case("é€A", "é€a"));
            require(!str::equals_ignore_ascii_case("É", "é"));
            require(!str::equals_ignore_ascii_case("a", "ab"));
            require(!str::equals_ignore_ascii_case("@", "`"));
            when str::equals_ignore_ascii_case("a", (1 / 0) as str) is Error::DivisionByZero {}
            else { crash("lost"); }
        }"#);
}

#[test]
fn array_equality_stops_at_the_first_difference() {
    run(r#"
        use array; use eq::Eq;
        var compared = 0;
        struct Item { id: int }
        extend Item with Eq[Item] {
            fn equals(self, other: Item) -> bool {
                compared += 1;
                if self.id < 0 { return Error::InvalidArgument("item"); }
                self.id == other.id
            }
        }
        fn main() {
            require(array::equals([1, 2], [1, 2]));
            require(array::equals(["é"], ["é"]));
            const empty: int[] = [];
            require(array::equals(empty, empty));
            require(!array::equals(empty, [1]));
            require(!array::equals([1], [1, 2]));
            require(!array::equals([1, 2], [1, 3]));
            require(!array::equals([Item(1), Item(2), Item(3)], [Item(1), Item(9), Item(3)]));
            require(compared == 2);
            compared = 0;
            require(!array::equals([Item(1)], [Item(1), Item(2)]));
            require(compared == 0);
            when array::equals([Item(-1)], [Item(1)]) is Error::InvalidArgument {}
            else { crash("comparison Error"); }
        }"#);
    rejects(
        "use array; struct S { x: int } fn main() { array::equals([S(1)], [S(1)]); }",
        "Eq",
    );
}

#[test]
fn append_all_extends_in_order_and_can_append_itself() {
    run(r#"
        use array;
        struct Box { value: int }
        struct Bag { items: str[] }
        fn main() {
            var items = [1, 2];
            items.append_all([3, 4])!!;
            require(array::equals(items, [1, 2, 3, 4]));
            const nothing: int[] = [];
            items.append_all(nothing)!!;
            require(items.len() == 4);
            items.append_all(items)!!;
            require(array::equals(items, [1, 2, 3, 4, 1, 2, 3, 4]));

            const alias = items;
            items.append_all(alias)!!;
            require(items.len() == 16 && alias.len() == 16);

            var bag = Bag(["é"]);
            bag.items.append_all(bag.items)!!;
            require(array::equals(bag.items, ["é", "é"]));

            const shared = Box(1);
            var boxes: Box[] = [];
            boxes.append_all([shared])!!;
            require(boxes[0]!!.value == 1);
        }"#);
    rejects(
        "use array; fn main() { var items = [1]; items.append_all([\"two\"]); }",
        "str",
    );
    rejects(
        "use array; fn main() { const items = [1]; items.append_all([2]); }",
        "items",
    );
}

#[test]
fn path_stem_removes_only_the_last_extension() {
    run(r#"
        use path;
        fn main() {
            require(path::stem("dir/archive.tar.gz") == "archive.tar");
            require(path::stem("dir/ångström.txt") == "ångström");
            require(path::stem("plain") == "plain");
            require(path::stem(".config") == ".config");
            require(path::stem("trailing.") == "trailing");
            require(path::stem("a.b/") == "a");
            require(path::stem("") == nil);
            require(path::stem("..") == nil);
            require(path::extension("a.txt") == "txt");
            when path::stem((1 / 0) as str) is Error::DivisionByZero {} else { crash("lost"); }
        }"#);
}

#[test]
fn seeded_floats_are_reproducible_and_below_one() {
    run(r#"
        use random;
        fn main() {
            var first = random::Seeded::new(42)!!;
            var second = random::Seeded::new(42)!!;
            var raw = random::Seeded::new(42)!!;
            for _ in 0..200 {
                const value: float = first.float()!!;
                require(value >= 0.0 && value < 1.0);
                require(second.float()!! == value);
                require((raw.next()!! as float)!! / 2147483648.0 == value);
            }
        }"#);
}

#[test]
fn synchronous_files_round_trip_text_bytes_and_lines() {
    let directory = Directory::new();
    let path = literal(&directory.file("notes-é.txt"));
    let large = literal(&directory.file("large.bin"));
    run(&format!(
        r#"
        use fs; use bytes; use array;
        fn main() {{
            fs::write_text({path}, "é\r\n\n€")!!;
            require(fs::read_text({path}) == "é\r\n\n€");
            require(array::equals(fs::read_lines({path})!!, ["é", "", "€"]));
            fs::append_text({path}, "!\n")!!;
            require(fs::read_text({path}) == "é\r\n\n€!\n");
            fs::append_bytes({path}, bytes::from_str("z")!!)!!;
            require(bytes::equals(fs::read_bytes({path})!!, bytes::from_str("é\r\n\n€!\nz")!!));
            fs::write_bytes({path}, bytes::new(0)!!)!!;
            require(fs::read_text({path}) == "");
            require(fs::read_lines({path})!!.len() == 0);
            require(bytes::len(fs::read_bytes({path})!!) == 0);
            fs::write_text({path}, "ab")!!;
            var writable = fs::read_bytes({path})!!;
            bytes::set(writable, 0, 65)!!;
            require(bytes::to_str(writable, 0, 2) == "Ab");
            fs::write_text({path}, "")!!;

            // Sizes around the first allocation and later doublings.
            for size in [1, 4095, 4096, 4097, 8192, 20000] {{
                var data = bytes::new(size)!!;
                for index in 0..size {{ bytes::set(data, index, (index * 7) % 251)!!; }}
                fs::write_bytes({large}, data)!!;
                require(bytes::equals(fs::read_bytes({large})!!, data));
            }}
            // The handles are closed: more calls than any handle quota succeed.
            for _ in 0..100 {{ require(fs::read_text({path}) == ""); }}
        }}"#
    ));
}

#[test]
fn synchronous_file_errors_are_values_without_partial_effects() {
    let directory = Directory::new();
    let kept = directory.file("kept.txt");
    std::fs::write(&kept, "kept").unwrap();
    let invalid = directory.file("invalid.bin");
    std::fs::write(&invalid, [0xff, b'\n']).unwrap();
    let kept = literal(&kept);
    let invalid = literal(&invalid);
    let missing = literal(&directory.file("missing/file"));
    let folder = literal(&directory.0);
    run(&format!(
        r#"
        use fs; use bytes;
        fn main() {{
            when fs::read_text({missing}) is Error::IoFailure(op, message) {{
                require(op == "io::open");
            }} else {{ crash("missing read"); }}
            when fs::read_lines({missing}) is Error::IoFailure {{}} else {{ crash("lines"); }}
            when fs::write_text({missing}, "x") is Error::IoFailure {{}} else {{ crash("write"); }}
            when fs::append_text({missing}, "x") is Error::IoFailure {{}} else {{ crash("append"); }}
            for _ in 0..100 {{
                when fs::read_bytes({folder}) is Error::IoFailure(op, message) {{
                    require(op == "io::read_into");
                }} else {{ crash("directory read"); }}
            }}
            when fs::read_text({invalid}) is Error::ParseFailure {{}} else {{ crash("utf8"); }}
            when fs::read_lines({invalid}) is Error::ParseFailure {{}} else {{ crash("utf8"); }}
            require(bytes::len(fs::read_bytes({invalid})!!) == 2);

            when fs::write_text({kept}, (1 / 0) as str) is Error::DivisionByZero {{}}
            else {{ crash("lost text Error"); }}
            when fs::write_bytes({kept}, bytes::new(-1)) is Error {{}}
            else {{ crash("lost data Error"); }}
            when fs::append_text({kept}, (1 / 0) as str) is Error::DivisionByZero {{}}
            else {{ crash("lost text Error"); }}
            when fs::write_text((1 / 0) as str, "x") is Error::DivisionByZero {{}}
            else {{ crash("lost path Error"); }}
            require(fs::read_text({kept}) == "kept");
        }}"#
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn synchronous_write_failures_reach_the_caller() {
    run(r#"
        use fs;
        fn main() {
            for _ in 0..100 {
                when fs::write_text("/dev/full", "x") is Error::IoWriteFailed {}
                else { crash("write failure lost"); }
            }
        }"#);
}

#[test]
fn synchronous_files_respect_capabilities() {
    let directory = Directory::new();
    let kept = directory.file("kept.txt");
    std::fs::write(&kept, "kept").unwrap();
    let path = literal(&kept);
    run_limited(
        &format!(
            r#"
            use fs;
            fn denied(value) {{
                when value is Error::PermissionDenied(op) {{ require(op == "io::open"); }}
                else {{ crash("capability bypassed"); }}
            }}
            fn main() {{
                try {{ denied(fs::read_text({path})); }} catch(error) {{ denied(error); }}
                try {{ denied(fs::write_text({path}, "x")); }} catch(error) {{ denied(error); }}
                try {{ denied(fs::append_text({path}, "x")); }} catch(error) {{ denied(error); }}
            }}"#
        ),
        VmCapabilities::restricted(),
        None,
    );
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "kept");
}

#[test]
fn synchronous_reads_use_the_whole_buffer_limit_and_no_more() {
    let directory = Directory::new();
    for limit in [1, 100, 4096, 10_000] {
        let exact = directory.file(&format!("exact-{limit}"));
        std::fs::write(&exact, vec![b'a'; limit]).unwrap();
        let over = directory.file(&format!("over-{limit}"));
        std::fs::write(&over, vec![b'a'; limit + 1]).unwrap();
        let exact = literal(&exact);
        let over = literal(&over);
        run_limited(
            &format!(
                r#"
                use fs; use bytes;
                fn limited(value) {{
                    when value is Error::ResourceLimit {{}} else {{ crash("limit bypassed"); }}
                }}
                fn main() {{
                    require(bytes::len(fs::read_bytes({exact})!!) == {limit});
                    try {{ limited(fs::read_bytes({over})); }} catch(error) {{ limited(error); }}
                    require(bytes::len(fs::read_bytes({exact})!!) == {limit});
                }}"#
            ),
            VmCapabilities::default(),
            Some(limit),
        );
    }
}
