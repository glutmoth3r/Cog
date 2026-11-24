// Differential soundness: verifier value facts versus the VM.
//
// The verifier's per-slot facts (known constants, type masks, may_error,
// callable targets) are not exposed to integration tests, so they are observed
// through branch pruning. Each generated program ends in a probe that branches
// on a predicate over a local's final value. An uninitialized-local read
// (`Load(99)`) is rejected by the verifier only when it considers the block
// reachable. The VM tells which side is really taken (a `Crash` placed in each
// side), and the verifier must never prune that side: the program with the
// trap on the taken side has to be rejected. Programs with the trap on the
// other side may be accepted or rejected, but if they validate they must run.

use cog::backend::{CallableSignature, CompiledProgram};
use cog::instruction::{Instruction as I, ValueType};
use cog::value::Value;
use cog::{bytecode, interpreter};

const FN_BASE: usize = 1_000_000;
const LOCALS: usize = 5;
const INTS: [i64; 8] = [0, 1, 2, 3, 7, -1, i64::MAX, i64::MIN];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 11) as usize % n
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Profile {
    Scalars,
    Aggregates,
    Calls,
}

struct Gen {
    rng: Rng,
    profile: Profile,
    code: Vec<I>,
    inited: Vec<usize>,
}

impl Gen {
    fn emit(&mut self, i: I) {
        self.code.push(i);
    }

    fn constant(&mut self) {
        let v = match self.rng.below(8) {
            0..=4 => Value::Int(INTS[self.rng.below(INTS.len())]),
            5 => Value::Bool(self.rng.below(2) == 0),
            6 => Value::Str(["", "a", "zz"][self.rng.below(3)].into()),
            _ => Value::Nil,
        };
        self.emit(I::Push(v));
    }

    fn int_const(&mut self) {
        let v = INTS[self.rng.below(INTS.len())];
        self.emit(I::Push(Value::Int(v)));
    }

    fn local(&mut self) {
        if self.inited.is_empty() {
            self.constant();
        } else {
            let l = self.inited[self.rng.below(self.inited.len())];
            self.emit(I::Load(l));
        }
    }

    fn int_or_expr(&mut self, depth: usize) {
        if self.rng.below(2) == 0 {
            self.int_const();
        } else {
            self.expr(depth - 1);
        }
    }

    /// Pushes exactly one value.
    fn expr(&mut self, depth: usize) {
        if depth == 0 || self.rng.below(4) == 0 {
            if self.rng.below(2) == 0 {
                self.local();
            } else {
                self.constant();
            }
            return;
        }
        let pick = match self.profile {
            Profile::Scalars => self.rng.below(6),
            Profile::Aggregates => 3 + self.rng.below(8),
            Profile::Calls => 6 + self.rng.below(9),
        };
        match pick {
            0 | 1 => {
                // Arithmetic, including overflow and division by zero.
                self.int_or_expr(depth);
                self.int_or_expr(depth);
                let op = [I::Add, I::Sub, I::Mul, I::Div, I::Mod][self.rng.below(5)].clone();
                self.emit(op);
            }
            2 => {
                self.expr(depth - 1);
                self.expr(depth - 1);
                let op = [I::Eq, I::Ne, I::Lt, I::Le, I::Gt, I::Ge][self.rng.below(6)].clone();
                self.emit(op);
            }
            3 => {
                self.expr(depth - 1);
                let t = [
                    ValueType::Int,
                    ValueType::Bool,
                    ValueType::Str,
                    ValueType::Nil,
                    ValueType::Error,
                ][self.rng.below(5)]
                .clone();
                self.emit(I::IsType(t));
                if self.rng.below(2) == 0 {
                    self.emit(I::Not);
                }
            }
            4 | 5 => self.branch(depth),
            6 => {
                // Tuple with an element read.
                let n = 1 + self.rng.below(3);
                for _ in 0..n {
                    self.expr(depth - 1);
                }
                self.emit(I::MakeTuple(n));
                let idx = self.rng.below(n + 1) as i64;
                self.emit(I::Push(Value::Int(idx)));
                self.emit(I::Index);
            }
            7 | 8 => {
                // Array with an element read, push and length.
                let n = self.rng.below(3);
                for _ in 0..n {
                    self.expr(depth - 1);
                }
                self.emit(I::MakeArray(n));
                match self.rng.below(3) {
                    0 => {
                        let idx = self.rng.below(n + 1) as i64;
                        self.emit(I::Push(Value::Int(idx)));
                        self.emit(I::Index);
                    }
                    1 => {
                        self.expr(depth - 1);
                        self.emit(I::ArrayPush);
                        self.emit(I::Len);
                    }
                    _ => self.emit(I::Len),
                }
            }
            9 => {
                // Tuple element replacement (possibly out of range).
                let n = 1 + self.rng.below(2);
                for _ in 0..n {
                    self.expr(depth - 1);
                }
                self.emit(I::MakeTuple(n));
                let k = self.rng.below(n + 1) as i64;
                self.emit(I::Push(Value::Int(k)));
                self.expr(depth - 1);
                self.emit(I::IndexSet);
            }
            10 | 11 => match self.rng.below(3) {
                0 => {
                    self.expr(depth - 1);
                    self.emit(I::Call(FN_BASE, 1));
                }
                1 => {
                    self.expr(depth - 1);
                    self.emit(I::Call(FN_BASE + 1, 1));
                }
                _ => {
                    self.int_or_expr(depth);
                    self.int_or_expr(depth);
                    self.emit(I::Call(FN_BASE + 2, 2));
                }
            },
            12 | 13 => {
                // Closure with a capture, called indirectly.
                self.expr(depth - 1);
                self.emit(I::MakeClosure(FN_BASE + 3, 1));
                self.emit(I::CallValue(0));
            }
            _ => {
                // Indirect call through a function value.
                self.expr(depth - 1);
                let f = [FN_BASE, FN_BASE + 1, FN_BASE + 4][self.rng.below(3)];
                self.emit(I::Push(Value::Function(f)));
                self.emit(I::CallValue(1));
            }
        }
    }

    fn branch(&mut self, depth: usize) {
        self.expr(depth - 1);
        let jif = self.code.len();
        self.emit(I::JumpIfFalse(0));
        self.expr(depth - 1);
        let jmp = self.code.len();
        self.emit(I::Jump(0));
        self.code[jif] = I::JumpIfFalse(self.code.len());
        self.expr(depth - 1);
        self.code[jmp] = I::Jump(self.code.len());
    }

    fn init(&mut self, l: usize) {
        if !self.inited.contains(&l) {
            self.inited.push(l);
        }
    }

    fn statement(&mut self) {
        let depth = 1 + self.rng.below(3);
        match self.rng.below(6) {
            0 if self.profile != Profile::Scalars && !self.inited.is_empty() => {
                // Aliasing mutation through a second local.
                let src = self.inited[self.rng.below(self.inited.len())];
                self.emit(I::Load(src));
                self.emit(I::Dup);
                let dst = self.rng.below(LOCALS);
                self.emit(I::Store(dst));
                let k = self.rng.below(3) as i64;
                self.emit(I::Push(Value::Int(k)));
                self.expr(1);
                self.emit(I::IndexSet);
                self.emit(I::Pop);
                self.init(dst);
            }
            1 if self.profile == Profile::Calls => {
                // Bounded loop accumulating into a local.
                let acc = self.rng.below(LOCALS);
                let counter = (acc + 1) % LOCALS;
                self.expr(1);
                self.emit(I::Store(acc));
                self.emit(I::Push(Value::Int(0)));
                self.emit(I::Store(counter));
                let head = self.code.len();
                self.emit(I::Load(counter));
                let k = 1 + self.rng.below(3) as i64;
                self.emit(I::Push(Value::Int(k)));
                self.emit(I::Lt);
                let exit = self.code.len();
                self.emit(I::JumpIfFalse(0));
                self.emit(I::Load(acc));
                self.int_const();
                let op = [I::Add, I::Mul, I::Sub][self.rng.below(3)].clone();
                self.emit(op);
                self.emit(I::Store(acc));
                self.emit(I::Load(counter));
                self.emit(I::Push(Value::Int(1)));
                self.emit(I::Add);
                self.emit(I::Store(counter));
                self.emit(I::Jump(head));
                self.code[exit] = I::JumpIfFalse(self.code.len());
                self.init(acc);
                self.init(counter);
            }
            _ => {
                let dst = self.rng.below(LOCALS);
                self.expr(depth);
                self.emit(I::Store(dst));
                self.init(dst);
            }
        }
    }
}

type Body = fn(usize) -> Vec<I>;

/// Appends the function bodies and resolves the placeholder targets.
fn finish(mut code: Vec<I>) -> CompiledProgram {
    let base = code.len();
    let bodies: [Body; 5] = [
        |_| {
            vec![
                I::Store(0),
                I::Load(0),
                I::Push(Value::Int(3)),
                I::Add,
                I::Return,
            ]
        },
        |b| {
            vec![
                I::Store(0),
                I::Load(0),
                I::Push(Value::Int(0)),
                I::Eq,
                I::JumpIfFalse(b + 7),
                I::Push(Value::Str("z".into())),
                I::Return,
                I::Load(0),
                I::Return,
            ]
        },
        |_| {
            vec![
                I::Store(1),
                I::Store(0),
                I::Load(0),
                I::Load(1),
                I::Div,
                I::Return,
            ]
        },
        |_| vec![I::LoadCapture(0), I::Push(Value::Int(1)), I::Add, I::Return],
        |_| {
            vec![
                I::Store(0),
                I::Load(0),
                I::IsType(ValueType::Int),
                I::Return,
            ]
        },
    ];
    let mut offsets = [0usize; 5];
    let mut tail = Vec::new();
    for (k, body) in bodies.iter().enumerate() {
        offsets[k] = base + tail.len();
        tail.extend(body(offsets[k]));
    }
    code.extend(tail);
    let remap = |t: usize| {
        if t >= FN_BASE {
            offsets[t - FN_BASE]
        } else {
            t
        }
    };
    for i in &mut code {
        match i {
            I::Call(t, n) => *i = I::Call(remap(*t), *n),
            I::MakeClosure(t, n) => *i = I::MakeClosure(remap(*t), *n),
            I::Push(Value::Function(t)) => *i = I::Push(Value::Function(remap(*t))),
            _ => {}
        }
    }
    let mut program = CompiledProgram::new(code);
    let shapes = [(1, 0), (1, 0), (2, 0), (0, 1), (1, 0)];
    for (k, (p, c)) in shapes.into_iter().enumerate() {
        program
            .callables
            .insert(offsets[k], CallableSignature::dynamic(p, c));
    }
    program
}

#[derive(Clone, Copy)]
enum Block {
    Benign,
    Crash,
    Trap,
}

fn probe_program(
    prefix: &[I],
    local: usize,
    test: &[I],
    on_true: Block,
    on_false: Block,
) -> CompiledProgram {
    let mut code = prefix.to_vec();
    code.push(I::Load(local));
    code.extend(test.iter().cloned());
    let jif = code.len();
    code.push(I::JumpIfFalse(0));
    let block = |code: &mut Vec<I>, b: Block| match b {
        Block::Benign => code.extend([I::Push(Value::Nil), I::Pop]),
        Block::Crash => code.extend([I::Push(Value::Nil), I::Crash(None)]),
        Block::Trap => code.extend([I::Load(99), I::Pop]),
    };
    block(&mut code, on_true);
    let jmp = code.len();
    code.push(I::Jump(0));
    code[jif] = I::JumpIfFalse(code.len());
    block(&mut code, on_false);
    code[jmp] = I::Jump(code.len());
    code.push(I::Halt);
    finish(code)
}

/// Runs in memory and after an encode/decode round trip; both must agree.
fn run_both(program: &CompiledProgram) -> bool {
    let direct = interpreter::run_with(program, &mut Vec::new()).is_ok();
    let decoded = bytecode::decode(&bytecode::encode(program).unwrap()).unwrap();
    let after = interpreter::run_with(&decoded, &mut Vec::new()).is_ok();
    assert_eq!(direct, after, "decode changed behavior");
    direct
}

fn tests() -> Vec<Vec<I>> {
    // The first test uses the raw value, which only decides for bool locals.
    let mut out = vec![vec![I::Dup, I::Pop]];
    for t in [
        ValueType::Int,
        ValueType::Bool,
        ValueType::Str,
        ValueType::Nil,
        ValueType::Error,
    ] {
        out.push(vec![I::IsType(t)]);
    }
    for v in INTS {
        out.push(vec![I::Push(Value::Int(v)), I::Eq]);
    }
    out.push(vec![I::Push(Value::Int(0)), I::Gt]);
    out.push(vec![I::Push(Value::Str("a".into())), I::Eq]);
    out.push(vec![I::Push(Value::Bool(true)), I::Eq]);
    out.push(vec![I::Push(Value::Nil), I::Ne]);
    out
}

pub fn run_profile(profile: Profile, seed: u64, programs: usize) {
    let mut rng = Rng(seed);
    let (mut runnable, mut pruned, mut probes) = (0usize, 0usize, 0usize);
    let tests = tests();
    let nop = [I::Pop, I::Push(Value::Nil), I::IsType(ValueType::Nil)];
    for _ in 0..programs {
        let mut g = Gen {
            rng: Rng(rng.next() | 1),
            profile,
            code: Vec::new(),
            inited: Vec::new(),
        };
        for _ in 0..1 + g.rng.below(4) {
            g.statement();
        }
        if g.inited.is_empty() {
            g.expr(1);
            g.emit(I::Store(0));
            g.init(0);
        }
        let prefix = g.code.clone();
        let base = probe_program(&prefix, g.inited[0], &nop, Block::Benign, Block::Benign);
        // Generated programs may legitimately fail at run time; only the
        // well-behaved ones are probed.
        let directly_runs = interpreter::run_with(&base, &mut Vec::new()).is_ok();
        if let Err(e) = bytecode::validate(&base) {
            assert!(!directly_runs, "verifier rejected a runnable program: {e}");
            continue;
        }
        if !run_both(&base) {
            continue;
        }
        runnable += 1;
        for &local in &g.inited {
            for (n, test) in tests.iter().enumerate() {
                let crash_true = probe_program(&prefix, local, test, Block::Crash, Block::Benign);
                let crash_false = probe_program(&prefix, local, test, Block::Benign, Block::Crash);
                if bytecode::validate(&crash_true).is_err() {
                    continue; // The verifier knows the probe is ill-typed.
                }
                let true_ok = run_both(&crash_true);
                let false_ok = run_both(&crash_false);
                if true_ok == false_ok {
                    continue; // No decidable outcome at run time.
                }
                probes += 1;
                let true_taken = !true_ok;
                let (taken, other) = if true_taken {
                    (
                        probe_program(&prefix, local, test, Block::Trap, Block::Benign),
                        probe_program(&prefix, local, test, Block::Benign, Block::Trap),
                    )
                } else {
                    (
                        probe_program(&prefix, local, test, Block::Benign, Block::Trap),
                        probe_program(&prefix, local, test, Block::Trap, Block::Benign),
                    )
                };
                assert!(
                    bytecode::validate(&taken).is_err(),
                    "verifier pruned a branch the VM takes (local {local}, test #{n}):\n{:?}",
                    taken.instructions
                );
                assert!(bytecode::encode(&taken).is_err());
                if bytecode::validate(&other).is_ok() {
                    pruned += 1;
                    assert!(run_both(&other), "validated probe failed at run time");
                }
            }
        }
    }
    assert!(
        runnable * 3 >= programs,
        "too few runnable programs: {runnable}/{programs}"
    );
    assert!(probes >= programs, "too few decidable probes: {probes}");
    assert!(
        pruned * 10 >= probes,
        "too little pruning to be a meaningful check: {pruned}/{probes}"
    );
    eprintln!("runnable={runnable} probes={probes} pruned={pruned}");
}
