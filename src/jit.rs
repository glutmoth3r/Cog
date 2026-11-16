//! Opt-in, profile-guided Cranelift OSR for simple synchronous bytecode loops.
//!
//! The JIT executes against an explicit frame of tagged slots. Opaque Cog
//! values stay rooted in the VM for the duration of the native call. Any
//! operation outside the lowered subset returns to the VM at that exact PC.

use crate::backend::CompiledProgram;
use crate::instruction::{Instruction, ValueType};
use crate::value::Value;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{AbiParam, BlockArg, InstBuilder, MemFlagsData, UserFuncName, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{
    JITBuilder, JITMemoryKind, JITMemoryProvider, JITModule, SystemMemoryProvider,
};
use cranelift_module::{Linkage, Module, default_libcall_names};
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem::{offset_of, size_of, transmute};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

const MAX_FUNCTION_INSTRUCTIONS: usize = 512;
const MAX_LOCALS: usize = 256;
const MAX_STACK: usize = 256;
const MAX_JIT_FUNCTIONS: usize = 128;
const MAX_JIT_CODE_BYTES: usize = 16 * 1024 * 1024;
const MAX_JIT_ENUM_VARIANTS: usize = 32;
const MAX_FRAME_ROOTS: usize = MAX_LOCALS + MAX_STACK;
const MAX_BOOTSTRAP_PROBES: usize = 3;
const WINDOW_BACKEDGES: u64 = 256;
const REQUIRED_WINDOWS: u32 = 3;
const MAX_HORIZON_NS: u64 = 1_000_000_000;
const MAX_ACTIVITY_GAP_NS: u64 = 1_000_000_000;
const TOKEN_CAPACITY_NS: u64 = 20_000_000;
const TOKEN_REFILL_PER_NS: f64 = 0.02;
const MAX_PROFILE_METRIC_NS: u64 = 60_000_000_000;
const APE_VERSION: u32 = 11;
static PROFILE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const EXIT_COMPLETED: i32 = 0;
const EXIT_RESUME: i32 = 1;
const EXIT_FAULT: i32 = 2;
const REASON_NORMAL: i64 = 0;
const REASON_PROFILE_SLICE: i64 = 1;

const TAG_UNINITIALIZED: u64 = 0;
const TAG_INT: u64 = 1;
const TAG_UINT: u64 = 2;
const TAG_BIGINT: u64 = 3;
const TAG_FLOAT: u64 = 4;
const TAG_BOOL: u64 = 5;
const TAG_CHAR: u64 = 6;
const TAG_STR: u64 = 7;
const TAG_ERROR: u64 = 8;
const TAG_NIL: u64 = 9;
const TAG_OPAQUE: u64 = 10;
const TAG_ARRAY: u64 = 11;
// Variant tags occupy a disjoint range while the payload continues to point
// at the rooted Cog value. This lets generated code compare nominal identity
// without copying or exposing the variant payload.
const TAG_VARIANT_BASE: u64 = 1 << 32;
// Small BigInts keep their original Cog value rooted. The tag carries the
// root index while the payload carries an exact signed or unsigned i64 value.
const TAG_BIGINT_SMALL_SIGNED_BASE: u64 = 2 << 32;
const TAG_BIGINT_SMALL_UNSIGNED_BASE: u64 = 3 << 32;
// Native arithmetic results have no input root, so restore them from payload.
const TAG_BIGINT_JIT_SIGNED: u64 = 12;
const TAG_BIGINT_JIT_UNSIGNED: u64 = 13;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JitExit {
    Completed,
    ResumeAt { pc: usize },
    FaultAt { pc: usize },
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct JitSlot {
    tag: u64,
    payload: u64,
}

#[repr(C)]
struct JitFrame {
    locals: *mut JitSlot,
    stack: *mut JitSlot,
    pc: u64,
    stack_len: u64,
    backedge_budget: u64,
    backedge_count: u64,
    exit_reason: i64,
}

type JitEntry = unsafe extern "C" fn(*mut JitFrame) -> i32;

#[derive(Clone)]
struct FunctionPlan {
    entry: usize,
    end: usize,
    instruction_count: usize,
    block_count: usize,
    edge_count: usize,
    phi_estimate: usize,
    local_count: usize,
    max_stack: usize,
    stack_depths: Vec<Option<usize>>,
    osr_entries: Vec<usize>,
}

#[derive(Debug, Clone, Default)]
struct VariantTags {
    names: Vec<String>,
    by_name: HashMap<String, u32>,
    enum_variants: HashMap<String, Vec<u32>>,
}

impl VariantTags {
    fn from_program(artifact: &CompiledProgram) -> Self {
        let mut names = std::collections::BTreeSet::new();
        for instruction in &artifact.instructions {
            match instruction {
                Instruction::Push(Value::TypeTag(name)) => {
                    let name = name.as_ref();
                    if is_user_variant_tag(name) {
                        names.insert(name.to_owned());
                    }
                }
                Instruction::IsType(ValueType::Variant(name)) => {
                    let name = name.as_ref();
                    if is_user_variant_tag(name) {
                        names.insert(name.to_owned());
                    }
                }
                _ => {}
            }
        }

        let names: Vec<_> = names.into_iter().collect();
        let mut by_name = HashMap::with_capacity(names.len());
        let mut enum_variants: HashMap<String, Vec<u32>> = HashMap::new();
        for (index, name) in names.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                // The verified bytecode limit makes this unreachable, but an
                // empty registry keeps the JIT conservative if limits change.
                return Self::default();
            };
            by_name.insert(name.clone(), index);
            if let Some((enum_name, _)) = name.rsplit_once("::") {
                enum_variants
                    .entry(enum_name.to_owned())
                    .or_default()
                    .push(index);
            }
        }
        Self {
            names,
            by_name,
            enum_variants,
        }
    }

    fn slot_tag(&self, name: &str) -> Option<u64> {
        self.by_name
            .get(name)
            .map(|index| TAG_VARIANT_BASE + u64::from(*index))
    }

    fn variant_tags(&self, value_type: &ValueType) -> Option<Vec<u64>> {
        match value_type {
            ValueType::Variant(name) => Some(vec![self.slot_tag(name)?]),
            ValueType::Enum(name) => {
                let variants = self.enum_variants.get(name.as_ref())?;
                if variants.is_empty() || variants.len() > MAX_JIT_ENUM_VARIANTS {
                    return None;
                }
                Some(
                    variants
                        .iter()
                        .map(|index| TAG_VARIANT_BASE + u64::from(*index))
                        .collect(),
                )
            }
            _ => None,
        }
    }
}

fn is_user_variant_tag(name: &str) -> bool {
    !name.starts_with("@struct::")
        && !name
            .strip_prefix(crate::error::ERROR_TYPE)
            .is_some_and(|tail| tail.starts_with("::"))
}

impl FunctionPlan {
    fn feature_vector(&self) -> [f64; 6] {
        [
            1.0,
            self.instruction_count as f64 / 8.0,
            self.block_count as f64,
            self.edge_count as f64,
            self.phi_estimate as f64 / 8.0,
            (self.local_count + self.max_stack) as f64 / 8.0,
        ]
    }

    fn upper_compile_ns(&self, coefficients: &Coefficients) -> u64 {
        coefficients.estimate_ns(self).saturating_mul(3).div_ceil(2)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Coefficients {
    a0: f64,
    instructions: f64,
    blocks: f64,
    edges: f64,
    parameters: f64,
    live_values: f64,
}

impl Coefficients {
    fn seeded() -> Self {
        Self {
            a0: 100_000.0,
            instructions: 3_000.0,
            blocks: 10_000.0,
            edges: 3_000.0,
            parameters: 1_000.0,
            live_values: 500.0,
        }
    }

    fn estimate_ns(&self, plan: &FunctionPlan) -> u64 {
        let x = plan.feature_vector();
        (self.a0
            + self.instructions * x[1]
            + self.blocks * x[2]
            + self.edges * x[3]
            + self.parameters * x[4]
            + self.live_values * x[5])
            .clamp(1_000.0, 1_000_000_000.0) as u64
    }

    fn update(&mut self, plan: &FunctionPlan, actual_ns: u64) {
        let x = plan.feature_vector();
        let predicted = self.estimate_ns(plan) as f64;
        let error = actual_ns as f64 - predicted;
        let denominator = 1.0 + x.iter().map(|value| value * value).sum::<f64>();
        let eta = 0.15;
        let coefficients = [
            &mut self.a0,
            &mut self.instructions,
            &mut self.blocks,
            &mut self.edges,
            &mut self.parameters,
            &mut self.live_values,
        ];
        for (coefficient, feature) in coefficients.into_iter().zip(x) {
            *coefficient = (*coefficient + eta * error * feature / denominator).max(0.0);
        }
    }

    fn is_valid(&self) -> bool {
        [
            self.a0,
            self.instructions,
            self.blocks,
            self.edges,
            self.parameters,
            self.live_values,
        ]
        .into_iter()
        .all(|value| value.is_finite() && (0.0..=1.0e12).contains(&value))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FunctionProfile {
    compile_ns: u64,
    vm_ns_per_backedge: u64,
    jit_ns_per_backedge: u64,
    #[serde(skip)]
    total_backedges: u64,
    #[serde(skip)]
    hot_series_ns: u64,
    #[serde(skip)]
    active_windows: u32,
    calibrated: bool,
}

impl FunctionProfile {
    fn is_valid(&self) -> bool {
        if [
            self.compile_ns,
            self.vm_ns_per_backedge,
            self.jit_ns_per_backedge,
        ]
        .into_iter()
        .any(|value| value > MAX_PROFILE_METRIC_NS)
        {
            return false;
        }
        if self.calibrated {
            self.compile_ns > 0 && self.vm_ns_per_backedge > 0 && self.jit_ns_per_backedge > 0
        } else {
            self.jit_ns_per_backedge == 0
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    key: String,
    coefficients: Option<Coefficients>,
    functions: BTreeMap<usize, FunctionProfile>,
}

struct Candidate {
    profile: FunctionProfile,
    window_edges: u64,
    windows_this_run: u32,
    window_started: Instant,
    last_backedge_at: Option<Instant>,
    last_selection_window: u32,
    retry_after_backedges: u64,
    compiled: Option<CompiledFunction>,
    disabled: bool,
}

impl Candidate {
    fn new(profile: FunctionProfile) -> Self {
        Self {
            profile,
            window_edges: 0,
            windows_this_run: 0,
            window_started: Instant::now(),
            last_backedge_at: None,
            last_selection_window: 0,
            retry_after_backedges: 0,
            compiled: None,
            disabled: false,
        }
    }
}

struct CompiledFunction {
    entry: JitEntry,
    probe_pending: bool,
}

#[derive(Debug, Clone, Copy)]
enum ApeDecision {
    Profile(&'static str),
    BootstrapCompile {
        priority: f64,
        estimated_cost_ns: u64,
    },
    Compile {
        priority: f64,
        estimated_cost_ns: u64,
    },
    Ineligible(&'static str),
    DeferBudget {
        estimated_cost_ns: u64,
        available_ns: i64,
    },
    SkipCacheFull,
}

fn log_ape_decision(entry: usize, decision: ApeDecision) {
    match decision {
        ApeDecision::Profile(reason) => {
            eprintln!("[jit] decision function={entry} action=profile reason={reason}");
        }
        ApeDecision::BootstrapCompile {
            priority,
            estimated_cost_ns,
        } => eprintln!(
            "[jit] decision function={entry} action=bootstrap priority={priority:.3} estimated_compile_ns={estimated_cost_ns}"
        ),
        ApeDecision::Compile {
            priority,
            estimated_cost_ns,
        } => eprintln!(
            "[jit] decision function={entry} action=compile priority={priority:.3} estimated_compile_ns={estimated_cost_ns}"
        ),
        ApeDecision::Ineligible(reason) => {
            eprintln!("[jit] decision function={entry} action=skip reason={reason}");
        }
        ApeDecision::DeferBudget {
            estimated_cost_ns,
            available_ns,
        } => eprintln!(
            "[jit] decision function={entry} action=defer-budget estimated_compile_ns={estimated_cost_ns} available_ns={available_ns}"
        ),
        ApeDecision::SkipCacheFull => {
            eprintln!("[jit] decision function={entry} action=skip reason=cache-full");
        }
    }
}

pub(crate) enum JitPlanState {
    NotInitialized,
    Unavailable,
    Ready(JitState),
}

pub(crate) struct JitState {
    plans: BTreeMap<usize, Arc<FunctionPlan>>,
    variant_tags: VariantTags,
    candidates: BTreeMap<usize, Candidate>,
    compiler: JitCompiler,
    coefficients: Coefficients,
    profile_key: String,
    profile_path: Option<PathBuf>,
    bootstrap_probes: usize,
    compile_tokens_ns: i64,
    token_refill_at: Instant,
}

impl JitState {
    pub(crate) fn new(
        artifact: &CompiledProgram,
        profile_path: Option<&Path>,
    ) -> Result<Self, String> {
        crate::bytecode::validate(artifact)
            .map_err(|_| "bytecode verification failed".to_owned())?;
        let variant_tags = VariantTags::from_program(artifact);
        let plans = analyze_program(artifact);
        if plans.is_empty() {
            return Err("no synchronous function has a supported empty-stack OSR point".into());
        }
        let (compiler, host_key) = JitCompiler::new()?;
        let encoded = crate::bytecode::encode(artifact).map_err(|error| error.to_string())?;
        let bytecode_hash = blake3::hash(&encoded).to_hex().to_string();
        let profile_key = format!(
            "{bytecode_hash}|abi={}|bytecode={}|ape={APE_VERSION}|cranelift={}|host={host_key}",
            crate::sdk::ABI_VERSION,
            crate::bytecode::VERSION,
            cranelift_jit::VERSION,
        );
        let loaded = profile_path.and_then(|path| load_profile(path, &profile_key));
        let coefficients = loaded
            .as_ref()
            .and_then(|profile| profile.coefficients.clone())
            .unwrap_or_else(Coefficients::seeded);
        let mut candidates = BTreeMap::new();
        for entry in plans.keys() {
            let profile = loaded
                .as_ref()
                .and_then(|profile| profile.functions.get(entry))
                .cloned()
                .unwrap_or_default();
            candidates.insert(*entry, Candidate::new(profile));
        }
        Ok(Self {
            plans,
            variant_tags,
            candidates,
            compiler,
            coefficients,
            profile_key,
            profile_path: profile_path.map(Path::to_path_buf),
            bootstrap_probes: 0,
            compile_tokens_ns: TOKEN_CAPACITY_NS as i64,
            token_refill_at: Instant::now(),
        })
    }

    pub(crate) fn contains(&self, entry: usize, target: usize) -> bool {
        self.plans
            .get(&entry)
            .is_some_and(|plan| plan.osr_entries.binary_search(&target).is_ok())
    }

    pub(crate) fn try_osr(
        &mut self,
        artifact: &CompiledProgram,
        entry: usize,
        target: usize,
        pc: &mut usize,
        locals: &mut Vec<Option<Value>>,
        bp: usize,
        stack: &mut Vec<Value>,
        operand_base: usize,
        debug: bool,
    ) -> Option<JitExit> {
        let plan = Arc::clone(self.plans.get(&entry)?);
        if !self.contains(entry, target) {
            return None;
        }
        let remaining = stack.len().checked_sub(operand_base)?;
        let expected_depth = plan
            .stack_depths
            .get(target.checked_sub(plan.entry)?)?
            .as_ref()
            .copied()?;
        if expected_depth != 0 || remaining != expected_depth {
            self.candidates.get_mut(&entry)?.disabled = true;
            if debug {
                eprintln!("[jit] ineligible function={entry} reason=nonempty-osr-stack");
            }
            return None;
        }
        let now = Instant::now();
        let window_closed = {
            let candidate = self.candidates.get_mut(&entry)?;
            if candidate.disabled {
                return None;
            }
            update_heat(candidate, now)
        };
        if debug && window_closed {
            if let Some(candidate) = self.candidates.get(&entry) {
                eprintln!(
                    "[jit] profile function={entry} backedges={} active_windows={} vm_ns_per_backedge={}",
                    candidate.profile.total_backedges,
                    candidate.profile.active_windows,
                    candidate.profile.vm_ns_per_backedge
                );
            }
        }
        self.refill_tokens(now);
        if self.candidates.get(&entry).is_some_and(|candidate| {
            candidate.compiled.is_some()
                && candidate.profile.total_backedges < candidate.retry_after_backedges
        }) {
            if debug && window_closed {
                eprintln!("[jit] decision function={entry} reason=fallback-cooldown");
            }
            return None;
        }

        if self
            .candidates
            .get(&entry)
            .is_some_and(|candidate| candidate.compiled.is_none())
        {
            let (bootstrap_probe, priority, estimated_cost) = match self.candidate_decision(entry) {
                ApeDecision::BootstrapCompile {
                    priority,
                    estimated_cost_ns,
                } => (true, priority, estimated_cost_ns),
                ApeDecision::Compile {
                    priority,
                    estimated_cost_ns,
                } => (false, priority, estimated_cost_ns),
                decision => {
                    if debug && window_closed {
                        log_ape_decision(entry, decision);
                    }
                    return None;
                }
            };
            let selection_window = self.candidates.get(&entry)?.windows_this_run;
            if self.candidates.get(&entry)?.last_selection_window == selection_window {
                return None;
            }
            let best_candidate = self.best_candidate(now);
            self.candidates.get_mut(&entry)?.last_selection_window = selection_window;
            if best_candidate.is_some_and(|best_entry| best_entry != entry) {
                if debug && window_closed {
                    log_ape_decision(
                        entry,
                        ApeDecision::Profile("higher-priority-candidate-active"),
                    );
                }
                return None;
            }
            if self.compiler.functions >= MAX_JIT_FUNCTIONS
                || self.compiler.cache_full.load(Ordering::Relaxed)
                || self.compiler.code_bytes.load(Ordering::Relaxed) >= MAX_JIT_CODE_BYTES
            {
                if debug && window_closed {
                    log_ape_decision(entry, ApeDecision::SkipCacheFull);
                }
                return None;
            }

            // An over-capacity compile reserves a full bucket and creates
            // debt when its measured cost is settled below.
            let reserved_cost =
                match reserve_compile_budget(&mut self.compile_tokens_ns, estimated_cost) {
                    Ok(reserved) => reserved,
                    Err(available_ns) => {
                        if debug && window_closed {
                            let decision = ApeDecision::DeferBudget {
                                estimated_cost_ns: estimated_cost,
                                available_ns,
                            };
                            log_ape_decision(entry, decision);
                        }
                        return None;
                    }
                };
            let started = Instant::now();
            match self.compiler.compile(&plan, artifact, &self.variant_tags) {
                Ok(function) => {
                    let actual = nanos(started.elapsed());
                    self.settle_compile_budget(reserved_cost, actual);
                    self.coefficients.update(&plan, actual);
                    if bootstrap_probe {
                        self.bootstrap_probes += 1;
                    }
                    let candidate = self.candidates.get_mut(&entry)?;
                    candidate.profile.compile_ns = actual;
                    candidate.compiled = Some(CompiledFunction {
                        entry: function,
                        // Measure this function's first native slice even when
                        // adaptive APE used a conservative cross-function rate.
                        probe_pending: true,
                    });
                    if debug {
                        eprintln!(
                            "[jit] {} function={entry} priority={priority:.3} compile_ns={actual} estimated_upper_ns={estimated_cost}",
                            if bootstrap_probe {
                                "bootstrap-compile"
                            } else {
                                "compile"
                            }
                        );
                    }
                    self.persist();
                }
                Err(error) => {
                    let actual = nanos(started.elapsed());
                    self.settle_compile_budget(reserved_cost, actual);
                    self.coefficients.update(&plan, actual);
                    if self.compiler.cache_full.load(Ordering::Relaxed) {
                        if debug {
                            log_ape_decision(entry, ApeDecision::SkipCacheFull);
                        }
                        return None;
                    }
                    let candidate = self.candidates.get_mut(&entry)?;
                    candidate.disabled = true;
                    if debug {
                        eprintln!("[jit] jit-failed function={entry} error={error}");
                    }
                    self.persist();
                    return None;
                }
            }
        }

        let (entry_function, probe_pending) = {
            let candidate = self.candidates.get(&entry)?;
            let compiled = candidate.compiled.as_ref()?;
            if candidate.profile.calibrated && adaptive_gain(candidate) <= 0.0 {
                return None;
            }
            (compiled.entry, compiled.probe_pending)
        };
        let mut roots = Vec::new();
        let mut jit_locals = vec![JitSlot::default(); plan.local_count];
        for (index, slot) in jit_locals.iter_mut().enumerate() {
            if let Some(value) = locals.get(bp + index).and_then(Option::as_ref) {
                *slot = to_slot(value, &mut roots, &self.variant_tags);
            }
        }
        if debug && probe_pending {
            eprintln!("[jit] osr function={entry} pc={target} locals={jit_locals:?}");
        }
        let mut jit_stack = vec![JitSlot::default(); plan.max_stack.max(remaining)];
        for (index, value) in stack[operand_base..].iter().enumerate() {
            jit_stack[index] = to_slot(value, &mut roots, &self.variant_tags);
        }
        let mut frame = JitFrame {
            locals: jit_locals.as_mut_ptr(),
            stack: jit_stack.as_mut_ptr(),
            pc: target as u64,
            stack_len: remaining as u64,
            backedge_budget: if probe_pending { WINDOW_BACKEDGES } else { 0 },
            backedge_count: 0,
            exit_reason: REASON_NORMAL,
        };
        let started = Instant::now();
        // The entry pointer remains owned by compiler.module for this plan's lifetime.
        let status = unsafe { (entry_function)(&mut frame) };
        let elapsed = nanos(started.elapsed());
        let exit = match decode_exit(&plan, artifact, &frame, status) {
            Ok(exit) => exit,
            Err(()) => {
                if let Some(candidate) = self.candidates.get_mut(&entry) {
                    candidate.disabled = true;
                }
                if debug {
                    eprintln!(
                        "[jit] fallback function={entry} reason=invalid-exit status={status} pc={} depth={} reason={}",
                        frame.pc, frame.stack_len, frame.exit_reason
                    );
                }
                *pc = target;
                return Some(JitExit::ResumeAt { pc: target });
            }
        };
        if restore_frame(
            &plan,
            &frame,
            &jit_locals,
            &jit_stack,
            &roots,
            &self.variant_tags,
            locals,
            bp,
            stack,
            operand_base,
        )
        .is_err()
        {
            if let Some(candidate) = self.candidates.get_mut(&entry) {
                candidate.disabled = true;
            }
            if debug {
                eprintln!("[jit] fallback function={entry} reason=invalid-returned-frame");
            }
            // The JIT only mutates its private copy of the frame. Resume from
            // the original OSR point and let the VM execute the loop again.
            *pc = target;
            return Some(JitExit::ResumeAt { pc: target });
        }
        *pc = frame.pc as usize;

        let backedges = frame.backedge_count;
        if debug {
            eprintln!(
                "[jit] exit function={entry} exit={exit:?} backedges={backedges} budget={} reason={} pc={} locals={jit_locals:?}",
                frame.backedge_budget, frame.exit_reason, frame.pc
            );
        }
        let complete_probe = probe_pending
            && backedges > 0
            && (frame.exit_reason == REASON_PROFILE_SLICE || matches!(exit, JitExit::Completed));
        let candidate = self.candidates.get_mut(&entry)?;
        if complete_probe {
            if let Some(compiled) = candidate.compiled.as_mut() {
                compiled.probe_pending = false;
            }
            candidate.profile.jit_ns_per_backedge = elapsed.saturating_div(backedges).max(1);
            candidate.profile.calibrated = true;
            let vm_ns = candidate.profile.vm_ns_per_backedge;
            if adaptive_gain(candidate) <= 0.0 {
                if debug {
                    eprintln!(
                        "[jit] calibrated-unprofitable function={entry} vm_ns_per_backedge={vm_ns} jit_ns_per_backedge={}",
                        candidate.profile.jit_ns_per_backedge
                    );
                }
            } else if debug {
                eprintln!(
                    "[jit] calibrated function={entry} vm_ns_per_backedge={vm_ns} jit_ns_per_backedge={}",
                    candidate.profile.jit_ns_per_backedge
                );
            }
            self.persist();
        } else if matches!(exit, JitExit::ResumeAt { .. })
            && frame.exit_reason != REASON_PROFILE_SLICE
        {
            candidate.retry_after_backedges = candidate
                .profile
                .total_backedges
                .saturating_add(WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS));
            if debug {
                eprintln!(
                    "[jit] fallback function={entry} exit={exit:?}; retry_after_backedges={}",
                    candidate.retry_after_backedges
                );
            }
        } else if matches!(exit, JitExit::FaultAt { .. }) {
            candidate.retry_after_backedges = candidate
                .profile
                .total_backedges
                .saturating_add(WINDOW_BACKEDGES);
            if debug {
                eprintln!(
                    "[jit] fault function={entry} exit={exit:?}; retry_after_backedges={}",
                    candidate.retry_after_backedges
                );
            }
        }
        Some(exit)
    }

    fn candidate_decision(&self, entry: usize) -> ApeDecision {
        let Some(candidate) = self.candidates.get(&entry) else {
            return ApeDecision::Ineligible("missing-candidate");
        };
        if candidate.disabled || candidate.compiled.is_some() {
            return ApeDecision::Ineligible("disabled-or-compiled");
        }
        let Some(plan) = self.plans.get(&entry) else {
            return ApeDecision::Ineligible("missing-plan");
        };
        if plan.instruction_count > MAX_FUNCTION_INSTRUCTIONS {
            return ApeDecision::Ineligible("function-too-large");
        }
        if candidate.windows_this_run < REQUIRED_WINDOWS
            || candidate.profile.active_windows < REQUIRED_WINDOWS
            || candidate.profile.total_backedges < WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS)
        {
            return ApeDecision::Profile("warming");
        }

        let estimate = plan.upper_compile_ns(&self.coefficients);
        if !candidate.profile.calibrated && self.bootstrap_probes < MAX_BOOTSTRAP_PROBES {
            let priority = candidate.profile.total_backedges as f64 / estimate.max(1) as f64;
            return ApeDecision::BootstrapCompile {
                priority,
                estimated_cost_ns: estimate,
            };
        }

        let predicted_jit_ns = if candidate.profile.calibrated {
            candidate.profile.jit_ns_per_backedge
        } else {
            let Some(estimate) =
                self.conservative_jit_estimate(candidate.profile.vm_ns_per_backedge)
            else {
                return ApeDecision::Profile("awaiting-shared-calibration");
            };
            estimate
        };
        let compile_upper_ns = if candidate.profile.compile_ns > 0 {
            candidate.profile.compile_ns.saturating_mul(3).div_ceil(2)
        } else {
            estimate
        };
        let gain = adaptive_gain_at(candidate, predicted_jit_ns, compile_upper_ns);
        if gain <= 0.0 {
            return ApeDecision::Profile("not-profitable");
        }
        ApeDecision::Compile {
            priority: gain / estimate.max(1) as f64,
            estimated_cost_ns: estimate,
        }
    }

    fn best_candidate(&self, now: Instant) -> Option<usize> {
        if self.compiler.functions >= MAX_JIT_FUNCTIONS
            || self.compiler.cache_full.load(Ordering::Relaxed)
            || self.compiler.code_bytes.load(Ordering::Relaxed) >= MAX_JIT_CODE_BYTES
        {
            return None;
        }
        let mut best: Option<(usize, f64)> = None;
        for (&entry, candidate) in &self.candidates {
            if candidate
                .last_backedge_at
                .is_none_or(|last| nanos(now.duration_since(last)) > MAX_ACTIVITY_GAP_NS)
            {
                continue;
            }
            let (priority, estimated_cost_ns) = match self.candidate_decision(entry) {
                ApeDecision::BootstrapCompile {
                    priority,
                    estimated_cost_ns,
                }
                | ApeDecision::Compile {
                    priority,
                    estimated_cost_ns,
                } => (priority, estimated_cost_ns),
                _ => continue,
            };
            if self.compile_tokens_ns < estimated_cost_ns.min(TOKEN_CAPACITY_NS) as i64 {
                continue;
            }
            if best.is_none_or(|(best_entry, best_priority)| {
                priority > best_priority || (priority == best_priority && entry < best_entry)
            }) {
                best = Some((entry, priority));
            }
        }
        best.map(|(entry, _)| entry)
    }

    fn conservative_jit_estimate(&self, vm_ns: u64) -> Option<u64> {
        if vm_ns == 0 {
            return None;
        }
        let conservative_speedup = self
            .candidates
            .values()
            .filter_map(|candidate| {
                let profile = &candidate.profile;
                (profile.calibrated
                    && profile.vm_ns_per_backedge > 0
                    && profile.jit_ns_per_backedge > 0)
                    .then_some(
                        profile.vm_ns_per_backedge as f64 / profile.jit_ns_per_backedge as f64,
                    )
            })
            .reduce(f64::min)?;
        if !conservative_speedup.is_finite() || conservative_speedup <= 1.0 {
            return None;
        }
        Some((vm_ns as f64 / conservative_speedup).ceil().max(1.0) as u64)
    }

    fn refill_tokens(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.token_refill_at).as_nanos() as f64;
        let refill = (elapsed * TOKEN_REFILL_PER_NS).min(u64::MAX as f64) as u64;
        self.compile_tokens_ns = self
            .compile_tokens_ns
            .saturating_add(refill.min(i64::MAX as u64) as i64)
            .min(TOKEN_CAPACITY_NS as i64);
        self.token_refill_at = now;
    }

    fn settle_compile_budget(&mut self, reserved_ns: u64, actual_ns: u64) {
        settle_compile_budget(&mut self.compile_tokens_ns, reserved_ns, actual_ns);
    }

    fn persist(&self) {
        let Some(path) = &self.profile_path else {
            return;
        };
        let profile = ProfileFile {
            key: self.profile_key.clone(),
            coefficients: Some(self.coefficients.clone()),
            functions: self
                .candidates
                .iter()
                .map(|(&entry, candidate)| (entry, candidate.profile.clone()))
                .collect(),
        };
        // Profiling is advisory: filesystem errors never affect VM execution.
        let _ = save_profile(path, &profile);
    }
}

fn adaptive_gain(candidate: &Candidate) -> f64 {
    let compile_upper_ns = candidate
        .profile
        .compile_ns
        .max(1)
        .saturating_mul(3)
        .div_ceil(2);
    adaptive_gain_at(
        candidate,
        candidate.profile.jit_ns_per_backedge,
        compile_upper_ns,
    )
}

fn adaptive_gain_at(candidate: &Candidate, jit_ns_per_backedge: u64, compile_upper_ns: u64) -> f64 {
    let vm = candidate.profile.vm_ns_per_backedge;
    let jit = jit_ns_per_backedge;
    if vm == 0 || jit == 0 || jit >= vm {
        return 0.0;
    }
    let horizon_ns = candidate.profile.hot_series_ns.min(MAX_HORIZON_NS).max(1);
    let rate = (1_000_000_000u64 / vm).max(1) / 2;
    let future_edges =
        ((rate as u128 * horizon_ns as u128) / 1_000_000_000).min(u64::MAX as u128) as u64;
    let savings = future_edges.saturating_mul(vm.saturating_sub(jit));
    savings.saturating_sub(compile_upper_ns) as f64
}

fn reserve_compile_budget(available_ns: &mut i64, estimate_ns: u64) -> Result<u64, i64> {
    let reserved_ns = estimate_ns.min(TOKEN_CAPACITY_NS);
    if *available_ns < reserved_ns as i64 {
        return Err(*available_ns);
    }
    *available_ns -= reserved_ns as i64;
    Ok(reserved_ns)
}

fn settle_compile_budget(available_ns: &mut i64, reserved_ns: u64, actual_ns: u64) {
    let adjustment = i128::from(reserved_ns) - i128::from(actual_ns);
    *available_ns = (i128::from(*available_ns) + adjustment)
        .clamp(i128::from(i64::MIN), i128::from(TOKEN_CAPACITY_NS as i64))
        as i64;
}

fn update_heat(candidate: &mut Candidate, now: Instant) -> bool {
    if candidate
        .last_backedge_at
        .is_some_and(|last| nanos(now.duration_since(last)) > MAX_ACTIVITY_GAP_NS)
    {
        candidate.window_edges = 0;
        candidate.windows_this_run = 0;
        candidate.last_selection_window = 0;
        candidate.profile.active_windows = 0;
        candidate.profile.hot_series_ns = 0;
    }
    candidate.last_backedge_at = Some(now);
    if candidate.window_edges == 0 {
        candidate.window_started = now;
    }
    candidate.window_edges += 1;
    candidate.profile.total_backedges = candidate.profile.total_backedges.saturating_add(1);
    if candidate.window_edges < WINDOW_BACKEDGES {
        return false;
    }
    let elapsed = nanos(now.duration_since(candidate.window_started));
    let measured = elapsed.saturating_div(WINDOW_BACKEDGES).max(1);
    candidate.profile.vm_ns_per_backedge = if candidate.profile.vm_ns_per_backedge == 0 {
        measured
    } else {
        measured
            .saturating_mul(3)
            .saturating_add(candidate.profile.vm_ns_per_backedge.saturating_mul(7))
            / 10
    };
    candidate.profile.hot_series_ns = candidate.profile.hot_series_ns.saturating_add(elapsed);
    candidate.profile.active_windows = candidate.profile.active_windows.saturating_add(1);
    candidate.windows_this_run = candidate.windows_this_run.saturating_add(1);
    candidate.window_edges = 0;
    candidate.window_started = now;
    true
}

fn analyze_program(artifact: &CompiledProgram) -> BTreeMap<usize, Arc<FunctionPlan>> {
    let entries: Vec<usize> = artifact.callables.keys().copied().collect();
    let mut plans = BTreeMap::new();
    for (index, entry) in entries.iter().copied().enumerate() {
        let signature = &artifact.callables[&entry];
        let end = entries
            .get(index + 1)
            .copied()
            .unwrap_or(artifact.instructions.len());
        if signature.asynchronous || signature.captures != 0 || entry >= end {
            continue;
        }
        if let Some(plan) = analyze_function(artifact, entry, end, signature.parameters) {
            if !plan.osr_entries.is_empty() {
                plans.insert(entry, Arc::new(plan));
            }
        }
    }
    plans
}

fn analyze_function(
    artifact: &CompiledProgram,
    entry: usize,
    end: usize,
    parameters: usize,
) -> Option<FunctionPlan> {
    let count = end.checked_sub(entry)?;
    if count == 0 || count > MAX_FUNCTION_INSTRUCTIONS {
        return None;
    }
    let mut local_count = parameters;
    let mut backedges = Vec::new();
    let mut blocks = std::collections::BTreeSet::from([entry]);
    let mut edge_count = 0usize;
    for pc in entry..end {
        let instruction = artifact.instructions.get(pc)?;
        if let Instruction::Load(index) | Instruction::Store(index) = instruction {
            local_count = local_count.max(index.checked_add(1)?);
        }
        if local_count > MAX_LOCALS {
            return None;
        }
        match instruction {
            Instruction::Jump(target) => {
                if !(entry..end).contains(target) {
                    return None;
                }
                blocks.insert(*target);
                edge_count += 1;
                if *target <= pc {
                    backedges.push((pc, *target));
                }
            }
            Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) => {
                if !(entry..end).contains(target) || pc + 1 >= end {
                    return None;
                }
                blocks.insert(*target);
                blocks.insert(pc + 1);
                edge_count += 2;
                if *target <= pc {
                    backedges.push((pc, *target));
                }
            }
            Instruction::JumpIfNoPropagation(target) => {
                if !(entry..end).contains(target) || pc + 1 >= end {
                    return None;
                }
                blocks.insert(*target);
                blocks.insert(pc + 1);
                edge_count += 2;
            }
            _ => {}
        }
    }
    if backedges.is_empty() {
        return None;
    }

    let mut depths = vec![None; count];
    depths[0] = Some(parameters);
    let mut max_stack = parameters;
    let mut queue = VecDeque::from([entry]);
    while let Some(pc) = queue.pop_front() {
        let depth = depths[pc - entry]?;
        let instruction = artifact.instructions.get(pc)?;
        let (required, produced) = instruction.stack_effect(&artifact.imports).ok()?;
        if depth < required {
            return None;
        }
        let next_depth = match instruction {
            Instruction::StackTruncate(target) => *target,
            _ => depth - required + produced,
        };
        max_stack = max_stack.max(next_depth);
        if max_stack > MAX_STACK {
            return None;
        }
        let successors = match instruction {
            Instruction::Jump(target) => [Some(*target), None],
            Instruction::JumpIfFalse(target)
            | Instruction::JumpIfTrue(target)
            | Instruction::JumpIfNoPropagation(target) => [Some(*target), Some(pc + 1)],
            Instruction::Return
            | Instruction::ReturnPropagate
            | Instruction::Crash(_)
            | Instruction::Halt => [None, None],
            _ if pc + 1 < end => [Some(pc + 1), None],
            _ => return None,
        };
        for successor in successors.into_iter().flatten() {
            if !(entry..end).contains(&successor) {
                return None;
            }
            let slot = &mut depths[successor - entry];
            match *slot {
                Some(existing) if existing != next_depth => return None,
                Some(_) => {}
                None => {
                    *slot = Some(next_depth);
                    queue.push_back(successor);
                }
            }
        }
    }
    let mut predecessors = HashMap::<usize, usize>::new();
    for pc in entry..end {
        if depths[pc - entry].is_none() {
            continue;
        }
        match &artifact.instructions[pc] {
            Instruction::Jump(target) => *predecessors.entry(*target).or_default() += 1,
            Instruction::JumpIfFalse(target)
            | Instruction::JumpIfTrue(target)
            | Instruction::JumpIfNoPropagation(target) => {
                *predecessors.entry(*target).or_default() += 1;
                *predecessors.entry(pc + 1).or_default() += 1;
            }
            Instruction::Return
            | Instruction::ReturnPropagate
            | Instruction::Crash(_)
            | Instruction::Halt => {}
            _ => *predecessors.entry(pc + 1).or_default() += 1,
        }
    }
    let phi_estimate = blocks
        .iter()
        .filter(|pc| predecessors.get(pc).copied().unwrap_or_default() > 1)
        .map(|pc| depths[pc - entry].unwrap_or_default() + local_count)
        .sum();
    let mut osr_entries: Vec<usize> = backedges
        .into_iter()
        .map(|(_, target)| target)
        .filter(|target| {
            depths
                .get(target - entry)
                .is_some_and(|depth| *depth == Some(0))
        })
        .collect();
    osr_entries.sort_unstable();
    osr_entries.dedup();
    Some(FunctionPlan {
        entry,
        end,
        instruction_count: count,
        block_count: blocks.len(),
        edge_count,
        phi_estimate,
        local_count,
        max_stack,
        stack_depths: depths,
        osr_entries,
    })
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

struct JitCompiler {
    module: JITModule,
    code_bytes: Arc<AtomicUsize>,
    cache_full: Arc<AtomicBool>,
    functions: usize,
    pointer_type: cranelift_codegen::ir::Type,
}

impl JitCompiler {
    fn new() -> Result<(Self, String), String> {
        let mut settings_builder = settings::builder();
        settings_builder
            .set("opt_level", "speed")
            .map_err(|error| error.to_string())?;
        let isa = cranelift_native::builder()
            .map_err(|error| error.to_string())?
            .finish(settings::Flags::new(settings_builder))
            .map_err(|error| error.to_string())?;
        let pointer_type = isa.pointer_type();
        if pointer_type != types::I64 {
            return Err("Cog JIT currently requires a 64-bit host".into());
        }
        let host = format!(
            "{}|{}",
            isa.triple(),
            isa.isa_flags()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        let code_bytes = Arc::new(AtomicUsize::new(0));
        let cache_full = Arc::new(AtomicBool::new(false));
        let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
        builder.memory_provider(Box::new(CountingMemoryProvider {
            inner: SystemMemoryProvider::new(),
            code_bytes: Arc::clone(&code_bytes),
            cache_full: Arc::clone(&cache_full),
            limit: MAX_JIT_CODE_BYTES,
        }));
        Ok((
            Self {
                module: JITModule::new(builder),
                code_bytes,
                cache_full,
                functions: 0,
                pointer_type,
            },
            host,
        ))
    }

    fn compile(
        &mut self,
        plan: &FunctionPlan,
        artifact: &CompiledProgram,
        variant_tags: &VariantTags,
    ) -> Result<JitEntry, String> {
        if self.functions >= MAX_JIT_FUNCTIONS {
            return Err("JIT function cache is full".into());
        }
        let function_id = self
            .module
            .declare_function(
                &format!("cog_jit_{}", plan.entry),
                Linkage::Local,
                &self.signature(),
            )
            .map_err(|error| error.to_string())?;
        let mut context = self.module.make_context();
        context.func.signature = self.signature();
        context.func.name = UserFuncName::user(0, function_id.as_u32());
        let mut builder_context = FunctionBuilderContext::new();
        lower_function(
            &mut self.module,
            &mut context,
            &mut builder_context,
            plan,
            artifact,
            variant_tags,
        )?;
        self.module
            .define_function(function_id, &mut context)
            .map_err(|error| error.to_string())?;
        self.module.clear_context(&mut context);
        self.module
            .finalize_definitions()
            .map_err(|error| error.to_string())?;
        self.functions += 1;
        let pointer = self.module.get_finalized_function(function_id);
        // JitModule owns this executable memory until JitCompiler is dropped.
        let function = unsafe { transmute::<*const u8, JitEntry>(pointer) };
        Ok(function)
    }

    fn signature(&self) -> cranelift_codegen::ir::Signature {
        let mut signature = self.module.make_signature();
        signature.params.push(AbiParam::new(self.pointer_type));
        signature.returns.push(AbiParam::new(types::I32));
        signature
    }
}

struct CountingMemoryProvider {
    inner: SystemMemoryProvider,
    code_bytes: Arc<AtomicUsize>,
    cache_full: Arc<AtomicBool>,
    limit: usize,
}

impl JITMemoryProvider for CountingMemoryProvider {
    fn allocate(&mut self, size: usize, align: u64, kind: JITMemoryKind) -> io::Result<*mut u8> {
        let executable = matches!(&kind, JITMemoryKind::Executable);
        if executable && self.code_bytes.load(Ordering::Relaxed).saturating_add(size) > self.limit {
            self.cache_full.store(true, Ordering::Relaxed);
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "Cog JIT executable memory limit reached",
            ));
        }
        let pointer = self.inner.allocate(size, align, kind)?;
        if executable {
            self.code_bytes.fetch_add(size, Ordering::Relaxed);
        }
        Ok(pointer)
    }

    unsafe fn free_memory(&mut self) {
        // The provider and all code pointers share the JITModule lifetime.
        unsafe { self.inner.free_memory() };
        self.code_bytes.store(0, Ordering::Relaxed);
    }

    fn finalize(
        &mut self,
        branch_protection: cranelift_jit::BranchProtection,
    ) -> cranelift_module::ModuleResult<()> {
        self.inner.finalize(branch_protection)
    }
}

fn to_slot(value: &Value, roots: &mut Vec<Value>, variant_tags: &VariantTags) -> JitSlot {
    match value {
        Value::Int(value) => JitSlot {
            tag: TAG_INT,
            payload: *value as u64,
        },
        Value::UInt(value) => JitSlot {
            tag: TAG_UINT,
            payload: *value,
        },
        Value::BigInt(integer) => small_bigint_slot(integer, value, roots)
            .unwrap_or_else(|| root_slot(value, roots, TAG_BIGINT)),
        Value::Float(value) => JitSlot {
            tag: TAG_FLOAT,
            payload: value.to_bits(),
        },
        Value::Bool(value) => JitSlot {
            tag: TAG_BOOL,
            payload: u64::from(*value),
        },
        Value::Char(value) => JitSlot {
            tag: TAG_CHAR,
            payload: *value as u32 as u64,
        },
        Value::Str(_) => root_slot(value, roots, TAG_STR),
        Value::Error(_) => root_slot(value, roots, TAG_ERROR),
        Value::Array(_) => root_slot(value, roots, TAG_ARRAY),
        Value::Variant(name, _) => variant_tags
            .slot_tag(name)
            .map(|tag| root_slot(value, roots, tag))
            .unwrap_or_else(|| root_slot(value, roots, TAG_OPAQUE)),
        Value::Nil => JitSlot {
            tag: TAG_NIL,
            payload: 0,
        },
        _ => root_slot(value, roots, TAG_OPAQUE),
    }
}

fn root_slot(value: &Value, roots: &mut Vec<Value>, tag: u64) -> JitSlot {
    let index = roots.len();
    roots.push(value.clone());
    JitSlot {
        tag,
        payload: index as u64,
    }
}

fn small_bigint_slot(
    integer: &num_bigint::BigInt,
    value: &Value,
    roots: &mut Vec<Value>,
) -> Option<JitSlot> {
    let (base, payload) = if let Some(value) = integer.to_i64() {
        (TAG_BIGINT_SMALL_SIGNED_BASE, value as u64)
    } else {
        (TAG_BIGINT_SMALL_UNSIGNED_BASE, integer.to_u64()?)
    };
    let index = roots.len();
    if index >= MAX_FRAME_ROOTS {
        return None;
    }
    let tag = base.checked_add(u64::try_from(index).ok()?)?;
    roots.push(value.clone());
    Some(JitSlot { tag, payload })
}

fn rooted_tag_index(tag: u64, base: u64) -> Option<usize> {
    let index = usize::try_from(tag.checked_sub(base)?).ok()?;
    (index < MAX_FRAME_ROOTS).then_some(index)
}

fn from_slot(
    slot: JitSlot,
    roots: &[Value],
    variant_tags: &VariantTags,
) -> Result<Option<Value>, ()> {
    let variant_value = |slot_tag: u64| {
        let index = usize::try_from(slot_tag.checked_sub(TAG_VARIANT_BASE)?).ok()?;
        if index >= variant_tags.names.len() {
            return None;
        }
        let expected_name = variant_tags.names.get(index)?;
        match roots.get(slot.payload as usize)? {
            value @ Value::Variant(name, _) if name.as_ref() == expected_name => {
                Some(value.clone())
            }
            _ => None,
        }
    };
    let value = match slot.tag {
        TAG_UNINITIALIZED => return Ok(None),
        TAG_INT => Value::Int(slot.payload as i64),
        TAG_UINT => Value::UInt(slot.payload),
        TAG_BIGINT => match roots.get(slot.payload as usize) {
            Some(value @ Value::BigInt(_)) => value.clone(),
            _ => return Err(()),
        },
        TAG_BIGINT_JIT_SIGNED => Value::BigInt(num_bigint::BigInt::from(slot.payload as i64)),
        TAG_BIGINT_JIT_UNSIGNED => Value::BigInt(num_bigint::BigInt::from(slot.payload)),
        tag if rooted_tag_index(tag, TAG_BIGINT_SMALL_SIGNED_BASE).is_some() => {
            let index = rooted_tag_index(tag, TAG_BIGINT_SMALL_SIGNED_BASE).ok_or(())?;
            match roots.get(index) {
                Some(value @ Value::BigInt(integer))
                    if integer
                        .to_i64()
                        .is_some_and(|value| value as u64 == slot.payload) =>
                {
                    value.clone()
                }
                _ => return Err(()),
            }
        }
        tag if rooted_tag_index(tag, TAG_BIGINT_SMALL_UNSIGNED_BASE).is_some() => {
            let index = rooted_tag_index(tag, TAG_BIGINT_SMALL_UNSIGNED_BASE).ok_or(())?;
            match roots.get(index) {
                Some(value @ Value::BigInt(integer))
                    if integer.to_u64().is_some_and(|value| value == slot.payload) =>
                {
                    value.clone()
                }
                _ => return Err(()),
            }
        }
        TAG_STR => match roots.get(slot.payload as usize) {
            Some(value @ Value::Str(_)) => value.clone(),
            _ => return Err(()),
        },
        TAG_ERROR => match roots.get(slot.payload as usize) {
            Some(value @ Value::Error(_)) => value.clone(),
            _ => return Err(()),
        },
        TAG_ARRAY => match roots.get(slot.payload as usize) {
            Some(value @ Value::Array(_)) => value.clone(),
            _ => return Err(()),
        },
        TAG_OPAQUE => roots.get(slot.payload as usize).ok_or(())?.clone(),
        tag if tag >= TAG_VARIANT_BASE
            && tag < TAG_VARIANT_BASE.saturating_add(variant_tags.names.len() as u64) =>
        {
            variant_value(tag).ok_or(())?
        }
        TAG_FLOAT => Value::Float(f64::from_bits(slot.payload)),
        TAG_BOOL if slot.payload <= 1 => Value::Bool(slot.payload != 0),
        TAG_CHAR => Value::Char(char::from_u32(slot.payload as u32).ok_or(())?),
        TAG_NIL if slot.payload == 0 => Value::Nil,
        _ => return Err(()),
    };
    Ok(Some(value))
}

fn decode_exit(
    plan: &FunctionPlan,
    artifact: &CompiledProgram,
    frame: &JitFrame,
    status: i32,
) -> Result<JitExit, ()> {
    let pc = usize::try_from(frame.pc).map_err(|_| ())?;
    let depth = usize::try_from(frame.stack_len).map_err(|_| ())?;
    let instruction = artifact.instructions.get(pc).ok_or(())?;
    if !(plan.entry..plan.end).contains(&pc)
        || plan.stack_depths.get(pc - plan.entry).copied().flatten() != Some(depth)
    {
        return Err(());
    }

    match (status, frame.exit_reason) {
        (EXIT_COMPLETED, REASON_NORMAL) if matches!(instruction, Instruction::Return) => {
            Ok(JitExit::Completed)
        }
        (EXIT_RESUME, REASON_NORMAL) => Ok(JitExit::ResumeAt { pc }),
        (EXIT_RESUME, REASON_PROFILE_SLICE)
            if depth == 0
                && frame.backedge_budget == 0
                && frame.backedge_count > 0
                && plan.osr_entries.binary_search(&pc).is_ok() =>
        {
            Ok(JitExit::ResumeAt { pc })
        }
        (EXIT_FAULT, REASON_NORMAL)
            if matches!(
                instruction,
                Instruction::Load(_)
                    | Instruction::Add
                    | Instruction::Sub
                    | Instruction::Mul
                    | Instruction::Div
                    | Instruction::Mod
            ) =>
        {
            Ok(JitExit::FaultAt { pc })
        }
        _ => Err(()),
    }
}

fn restore_frame(
    plan: &FunctionPlan,
    frame: &JitFrame,
    jit_locals: &[JitSlot],
    jit_stack: &[JitSlot],
    roots: &[Value],
    variant_tags: &VariantTags,
    locals: &mut Vec<Option<Value>>,
    bp: usize,
    stack: &mut Vec<Value>,
    operand_base: usize,
) -> Result<(), ()> {
    let pc = usize::try_from(frame.pc).map_err(|_| ())?;
    let depth = usize::try_from(frame.stack_len).map_err(|_| ())?;
    if !(plan.entry..plan.end).contains(&pc)
        || jit_locals.len() != plan.local_count
        || depth > jit_stack.len()
        || plan.stack_depths.get(pc - plan.entry).copied().flatten() != Some(depth)
    {
        return Err(());
    }

    // Decode every slot before touching the interpreter frame. If native code
    // ever returns an invalid tag or root index, the caller can safely restart
    // this pure loop from its original OSR state.
    let mut restored_locals = Vec::new();
    restored_locals
        .try_reserve_exact(jit_locals.len())
        .map_err(|_| ())?;
    for slot in jit_locals.iter().copied() {
        restored_locals.push(from_slot(slot, roots, variant_tags)?);
    }
    let mut restored_stack = Vec::new();
    restored_stack.try_reserve_exact(depth).map_err(|_| ())?;
    for slot in jit_stack.iter().copied().take(depth) {
        restored_stack.push(from_slot(slot, roots, variant_tags)?.ok_or(())?);
    }

    let local_end = bp.checked_add(plan.local_count).ok_or(())?;
    let stack_end = operand_base.checked_add(depth).ok_or(())?;
    if locals.len() < local_end {
        locals
            .try_reserve(local_end - locals.len())
            .map_err(|_| ())?;
    }
    if stack.len() < stack_end {
        stack.try_reserve(stack_end - stack.len()).map_err(|_| ())?;
    }
    if locals.len() < local_end {
        locals.resize_with(local_end, || None);
    }
    for (index, value) in restored_locals.into_iter().enumerate() {
        locals[bp + index] = value;
    }
    stack.truncate(operand_base);
    stack.extend(restored_stack);
    Ok(())
}

fn load_profile(path: &Path, expected_key: &str) -> Option<ProfileFile> {
    let bytes = fs::read(path).ok()?;
    let mut profile: ProfileFile = serde_json::from_slice(&bytes).ok()?;
    if profile.key != expected_key {
        return None;
    }
    if profile
        .coefficients
        .as_ref()
        .is_some_and(|coefficients| !coefficients.is_valid())
    {
        profile.coefficients = None;
    }
    profile
        .functions
        .retain(|_, measurement| measurement.is_valid());
    Some(profile)
}

fn save_profile(path: &Path, profile: &ProfileFile) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let encoded = serde_json::to_vec(profile).map_err(io::Error::other)?;
    let (temporary, mut file) = (0..8)
        .find_map(|_| {
            let sequence = PROFILE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary = path.with_extension(format!("tmp-{}-{sequence}", std::process::id()));
            match OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
            {
                Ok(file) => Some(Ok((temporary, file))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()?
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::AlreadyExists, "profile temp path collision")
        })?;
    let write_result = file.write_all(&encoded).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn jit_reachable_pcs(
    plan: &FunctionPlan,
    artifact: &CompiledProgram,
    variant_tags: &VariantTags,
) -> Vec<bool> {
    let count = plan.end - plan.entry;
    let mut reachable = vec![false; count];
    let mut queue = VecDeque::new();
    for &entry in &plan.osr_entries {
        if (plan.entry..plan.end).contains(&entry) && !reachable[entry - plan.entry] {
            reachable[entry - plan.entry] = true;
            queue.push_back(entry);
        }
    }

    while let Some(pc) = queue.pop_front() {
        let Some(instruction) = artifact.instructions.get(pc) else {
            continue;
        };
        let next = (pc + 1 < plan.end).then_some(pc + 1);
        let successors: Vec<usize> = match instruction {
            Instruction::Push(value) if constant_slot(value).is_some() => {
                next.into_iter().collect()
            }
            Instruction::Load(_)
            | Instruction::Store(_)
            | Instruction::Add
            | Instruction::Sub
            | Instruction::Mul
            | Instruction::Div
            | Instruction::Mod
            | Instruction::Eq
            | Instruction::Ne
            | Instruction::Lt
            | Instruction::Le
            | Instruction::Gt
            | Instruction::Ge
            | Instruction::And
            | Instruction::Or
            | Instruction::Xor
            | Instruction::Not
            | Instruction::Dup
            | Instruction::Pop
            | Instruction::Swap
            | Instruction::ConstrainArrayFrom => next.into_iter().collect(),
            Instruction::IsType(value_type)
                if type_tag(value_type).is_some()
                    || variant_tags.variant_tags(value_type).is_some() =>
            {
                next.into_iter().collect()
            }
            Instruction::Jump(target) => vec![*target],
            Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) => {
                let mut successors = vec![*target];
                successors.extend(next);
                successors
            }
            Instruction::Return | Instruction::ReturnPropagate => Vec::new(),
            _ => Vec::new(),
        };
        for successor in successors {
            if !(plan.entry..plan.end).contains(&successor) {
                continue;
            }
            let relative = successor - plan.entry;
            if plan.stack_depths[relative].is_some() && !reachable[relative] {
                reachable[relative] = true;
                queue.push_back(successor);
            }
        }
    }
    reachable
}

struct JitLocalState {
    variables: Vec<(Variable, Variable)>,
    dirty: Variable,
    exit_block: cranelift_codegen::ir::Block,
}

#[derive(Clone, Copy)]
struct JitSite {
    pc: usize,
    stack_depth: usize,
}

fn lower_function(
    module: &mut JITModule,
    context: &mut cranelift_codegen::Context,
    builder_context: &mut FunctionBuilderContext,
    plan: &FunctionPlan,
    artifact: &CompiledProgram,
    variant_tags: &VariantTags,
) -> Result<(), String> {
    let mut builder = FunctionBuilder::new(&mut context.func, builder_context);
    let dispatch = builder.create_block();
    let exit_block = builder.create_block();
    builder.append_block_param(exit_block, types::I32);
    builder.append_block_param(exit_block, types::I64);
    builder.append_block_param(exit_block, types::I64);
    builder.append_block_param(exit_block, types::I64);
    builder.append_block_params_for_function_params(dispatch);
    let frame = builder.block_params(dispatch)[0];
    let local_state = JitLocalState {
        variables: (0..plan.local_count)
            .map(|_| {
                (
                    builder.declare_var(types::I64),
                    builder.declare_var(types::I64),
                )
            })
            .collect(),
        dirty: builder.declare_var(types::I8),
        exit_block,
    };
    let reachable = jit_reachable_pcs(plan, artifact, variant_tags);
    let mut blocks = HashMap::new();
    for (relative, depth) in plan.stack_depths.iter().enumerate() {
        if depth.is_some() && reachable[relative] {
            blocks.insert(plan.entry + relative, builder.create_block());
        }
    }

    builder.switch_to_block(dispatch);
    let start_pc = load_frame(
        &mut builder,
        frame,
        offset_of!(JitFrame, pc) as i32,
        types::I64,
    );
    let clean = builder.ins().iconst(types::I8, 0);
    builder.def_var(local_state.dirty, clean);
    for (index, (tag_variable, payload_variable)) in local_state.variables.iter().enumerate() {
        let tag = load_slot_field(&mut builder, frame, false, index, 0);
        let payload = load_slot_field(&mut builder, frame, false, index, 8);
        builder.def_var(*tag_variable, tag);
        builder.def_var(*payload_variable, payload);
    }
    for target in &plan.osr_entries {
        let target_block = blocks
            .get(target)
            .copied()
            .ok_or("OSR point has no bytecode block")?;
        let is_target = builder
            .ins()
            .icmp_imm_s(IntCC::Equal, start_pc, *target as i64);
        let next = builder.create_block();
        builder.ins().brif(is_target, target_block, &[], next, &[]);
        builder.switch_to_block(next);
    }
    let fallback = builder.ins().iconst(types::I32, i64::from(EXIT_RESUME));
    builder.ins().return_(&[fallback]);

    for pc in plan.entry..plan.end {
        let relative = pc - plan.entry;
        let Some(depth) = plan.stack_depths[relative].filter(|_| reachable[relative]) else {
            continue;
        };
        builder.switch_to_block(blocks[&pc]);
        let next = blocks.get(&(pc + 1)).copied();
        match &artifact.instructions[pc] {
            Instruction::Push(value) => {
                if let Some(slot) = constant_slot(value) {
                    store_slot_const(&mut builder, frame, true, depth, slot);
                    jump_to(&mut builder, next, "push fallthrough")?;
                } else {
                    emit_exit(
                        &mut builder,
                        frame,
                        &local_state,
                        pc,
                        depth,
                        EXIT_RESUME,
                        REASON_NORMAL,
                    );
                }
            }
            Instruction::Load(index) => {
                let (tag_variable, payload_variable) = local_state.variables[*index];
                let tag = builder.use_var(tag_variable);
                let initialized =
                    builder
                        .ins()
                        .icmp_imm_s(IntCC::NotEqual, tag, TAG_UNINITIALIZED as i64);
                let valid = builder.create_block();
                let invalid = builder.create_block();
                builder.ins().brif(initialized, valid, &[], invalid, &[]);
                builder.switch_to_block(invalid);
                emit_exit(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    EXIT_FAULT,
                    REASON_NORMAL,
                );
                builder.switch_to_block(valid);
                let payload = builder.use_var(payload_variable);
                store_slot_field(&mut builder, frame, true, depth, 0, tag);
                store_slot_field(&mut builder, frame, true, depth, 8, payload);
                jump_to(&mut builder, next, "load fallthrough")?;
            }
            Instruction::Store(index) => {
                let (tag_variable, payload_variable) = local_state.variables[*index];
                let tag = load_slot_field(&mut builder, frame, true, depth - 1, 0);
                let payload = load_slot_field(&mut builder, frame, true, depth - 1, 8);
                builder.def_var(tag_variable, tag);
                builder.def_var(payload_variable, payload);
                let dirty = builder.ins().iconst(types::I8, 1);
                builder.def_var(local_state.dirty, dirty);
                jump_to(&mut builder, next, "store fallthrough")?;
            }
            Instruction::Add
            | Instruction::Sub
            | Instruction::Mul
            | Instruction::Div
            | Instruction::Mod => {
                emit_numeric_binary(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    next.ok_or("binary fallthrough is missing")?,
                    &artifact.instructions[pc],
                );
            }
            Instruction::Eq
            | Instruction::Ne
            | Instruction::Lt
            | Instruction::Le
            | Instruction::Gt
            | Instruction::Ge => {
                emit_numeric_compare(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    next.ok_or("comparison fallthrough is missing")?,
                    &artifact.instructions[pc],
                );
            }
            Instruction::Jump(target) => {
                let target_block = blocks
                    .get(target)
                    .copied()
                    .ok_or("jump target has no bytecode block")?;
                emit_jump(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    *target,
                    plan.stack_depths[*target - plan.entry].ok_or("invalid jump stack")?,
                    target_block,
                );
            }
            Instruction::JumpIfFalse(target) | Instruction::JumpIfTrue(target) => {
                emit_conditional_jump(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    *target,
                    next,
                    &blocks,
                    plan,
                    &artifact.instructions[pc],
                )?;
            }
            Instruction::Return => {
                emit_exit(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    EXIT_COMPLETED,
                    REASON_NORMAL,
                );
            }
            // Propagating returns update interpreter-only error state. Resume
            // before the instruction so the VM preserves those semantics.
            Instruction::ReturnPropagate => {
                emit_exit(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    EXIT_RESUME,
                    REASON_NORMAL,
                );
            }
            Instruction::IsType(value_type) => {
                if let Some(expected_tag) = type_tag(value_type) {
                    let actual_tag = load_slot_field(&mut builder, frame, true, depth - 1, 0);
                    let condition = if matches!(value_type, ValueType::BigInt) {
                        let rooted_bigint =
                            builder
                                .ins()
                                .icmp_imm_s(IntCC::Equal, actual_tag, TAG_BIGINT as i64);
                        let small_bigint = small_bigint_tag_test(&mut builder, actual_tag);
                        builder.ins().bor(rooted_bigint, small_bigint)
                    } else {
                        builder
                            .ins()
                            .icmp_imm_s(IntCC::Equal, actual_tag, expected_tag as i64)
                    };
                    let payload = builder.ins().uextend(types::I64, condition);
                    store_slot_const(
                        &mut builder,
                        frame,
                        true,
                        depth - 1,
                        JitSlot {
                            tag: TAG_BOOL,
                            payload: 0,
                        },
                    );
                    store_slot_field(&mut builder, frame, true, depth - 1, 8, payload);
                    jump_to(&mut builder, next, "type-test fallthrough")?;
                } else if matches!(value_type, ValueType::Enum(_) | ValueType::Variant(_)) {
                    emit_variant_type_test(
                        &mut builder,
                        frame,
                        &local_state,
                        JitSite {
                            pc,
                            stack_depth: depth,
                        },
                        next,
                        value_type,
                        variant_tags,
                    )?;
                } else {
                    emit_exit(
                        &mut builder,
                        frame,
                        &local_state,
                        pc,
                        depth,
                        EXIT_RESUME,
                        REASON_NORMAL,
                    );
                }
            }
            Instruction::And | Instruction::Or | Instruction::Xor => {
                emit_bool_binary(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    next.ok_or("boolean operation fallthrough is missing")?,
                    &artifact.instructions[pc],
                );
            }
            Instruction::Not => {
                let tag = load_slot_field(&mut builder, frame, true, depth - 1, 0);
                let is_bool = builder.ins().icmp_imm_s(IntCC::Equal, tag, TAG_BOOL as i64);
                let valid = builder.create_block();
                let invalid = builder.create_block();
                builder.ins().brif(is_bool, valid, &[], invalid, &[]);
                builder.switch_to_block(invalid);
                emit_exit(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    EXIT_RESUME,
                    REASON_NORMAL,
                );
                builder.switch_to_block(valid);
                let payload = load_slot_field(&mut builder, frame, true, depth - 1, 8);
                let is_false = builder.ins().icmp_imm_s(IntCC::Equal, payload, 0);
                let payload = builder.ins().uextend(types::I64, is_false);
                store_slot_field(&mut builder, frame, true, depth - 1, 8, payload);
                jump_to(&mut builder, next, "not fallthrough")?;
            }
            Instruction::Dup => {
                copy_slot(&mut builder, frame, true, depth - 1, true, depth);
                jump_to(&mut builder, next, "dup fallthrough")?;
            }
            Instruction::Pop => jump_to(&mut builder, next, "pop fallthrough")?,
            Instruction::Swap => {
                let a_tag = load_slot_field(&mut builder, frame, true, depth - 1, 0);
                let a_payload = load_slot_field(&mut builder, frame, true, depth - 1, 8);
                let b_tag = load_slot_field(&mut builder, frame, true, depth - 2, 0);
                let b_payload = load_slot_field(&mut builder, frame, true, depth - 2, 8);
                store_slot_field(&mut builder, frame, true, depth - 1, 0, b_tag);
                store_slot_field(&mut builder, frame, true, depth - 1, 8, b_payload);
                store_slot_field(&mut builder, frame, true, depth - 2, 0, a_tag);
                store_slot_field(&mut builder, frame, true, depth - 2, 8, a_payload);
                jump_to(&mut builder, next, "swap fallthrough")?;
            }
            Instruction::ConstrainArrayFrom => {
                // For scalars the VM operation preserves the assigned value.
                // Arrays may carry mutable type constraints, so let the VM own it.
                let tag = load_slot_field(&mut builder, frame, true, depth - 2, 0);
                let is_array = builder
                    .ins()
                    .icmp_imm_s(IntCC::Equal, tag, TAG_ARRAY as i64);
                let fallback = builder.create_block();
                let scalar = builder.create_block();
                builder.ins().brif(is_array, fallback, &[], scalar, &[]);
                builder.switch_to_block(fallback);
                emit_exit(
                    &mut builder,
                    frame,
                    &local_state,
                    pc,
                    depth,
                    EXIT_RESUME,
                    REASON_NORMAL,
                );
                builder.switch_to_block(scalar);
                set_stack_len(&mut builder, frame, depth - 1);
                jump_to(&mut builder, next, "array-context fallthrough")?;
            }
            _ => emit_exit(
                &mut builder,
                frame,
                &local_state,
                pc,
                depth,
                EXIT_RESUME,
                REASON_NORMAL,
            ),
        }
    }
    builder.switch_to_block(local_state.exit_block);
    let exit_params = builder.block_params(local_state.exit_block);
    let exit_status = exit_params[0];
    let exit_reason = exit_params[1];
    let exit_pc = exit_params[2];
    let exit_depth = exit_params[3];
    let dirty = builder.use_var(local_state.dirty);
    let flush = builder.create_block();
    let ready = builder.create_block();
    builder.ins().brif(dirty, flush, &[], ready, &[]);

    builder.switch_to_block(flush);
    for (index, (tag_variable, payload_variable)) in local_state.variables.iter().enumerate() {
        let tag = builder.use_var(*tag_variable);
        let payload = builder.use_var(*payload_variable);
        store_slot_field(&mut builder, frame, false, index, 0, tag);
        store_slot_field(&mut builder, frame, false, index, 8, payload);
    }
    builder.ins().jump(ready, &[]);

    builder.switch_to_block(ready);
    for (offset, value) in [
        (offset_of!(JitFrame, pc) as i32, exit_pc),
        (offset_of!(JitFrame, stack_len) as i32, exit_depth),
        (offset_of!(JitFrame, exit_reason) as i32, exit_reason),
    ] {
        let address = builder.ins().iadd_imm_s(frame, i64::from(offset));
        builder
            .ins()
            .store(MemFlagsData::trusted(), value, address, 0);
    }
    builder.ins().return_(&[exit_status]);
    builder.seal_all_blocks();
    builder.finalize(module.target_config());
    Ok(())
}

fn emit_variant_type_test(
    builder: &mut FunctionBuilder,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    site: JitSite,
    next: Option<cranelift_codegen::ir::Block>,
    value_type: &ValueType,
    variant_tags: &VariantTags,
) -> Result<(), String> {
    let Some(expected_tags) = variant_tags.variant_tags(value_type) else {
        emit_exit(
            builder,
            frame,
            local_state,
            site.pc,
            site.stack_depth,
            EXIT_RESUME,
            REASON_NORMAL,
        );
        return Ok(());
    };
    let next = next.ok_or("variant type-test fallthrough is missing")?;
    let actual_tag = load_slot_field(builder, frame, true, site.stack_depth - 1, 0);
    // Unknown variants and Error values may carry a nominal tag that is not
    // present in this artifact's registry. Resume the VM so it can compare the
    // full Cog tag instead of treating an absent native tag as a false match.
    let is_opaque = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, actual_tag, TAG_OPAQUE as i64);
    let is_error = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, actual_tag, TAG_ERROR as i64);
    let must_resume = builder.ins().bor(is_opaque, is_error);
    let fallback = builder.create_block();
    let inspect = builder.create_block();
    builder.ins().brif(must_resume, fallback, &[], inspect, &[]);

    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        site.pc,
        site.stack_depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );

    builder.switch_to_block(inspect);
    let mut matches = None;
    for expected_tag in expected_tags {
        let match_tag = builder.ins().icmp_imm_s(
            IntCC::Equal,
            actual_tag,
            i64::try_from(expected_tag).map_err(|_| "variant tag exceeds JIT immediate")?,
        );
        matches = Some(match matches {
            Some(previous) => builder.ins().bor(previous, match_tag),
            None => match_tag,
        });
    }
    let matches = matches.ok_or("variant test has no registered tags")?;
    let payload = builder.ins().uextend(types::I64, matches);
    store_slot_const(
        builder,
        frame,
        true,
        site.stack_depth - 1,
        JitSlot {
            tag: TAG_BOOL,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, site.stack_depth - 1, 8, payload);
    jump_to(builder, Some(next), "variant type-test fallthrough")
}

fn type_tag(value_type: &ValueType) -> Option<u64> {
    Some(match value_type {
        ValueType::Int => TAG_INT,
        ValueType::UInt => TAG_UINT,
        ValueType::BigInt => TAG_BIGINT,
        ValueType::Float => TAG_FLOAT,
        ValueType::Bool => TAG_BOOL,
        ValueType::Char => TAG_CHAR,
        ValueType::Str => TAG_STR,
        ValueType::Nil => TAG_NIL,
        ValueType::Error => TAG_ERROR,
        ValueType::Enum(_) | ValueType::Variant(_) => return None,
    })
}

fn constant_slot(value: &Value) -> Option<JitSlot> {
    Some(match value {
        Value::Int(value) => JitSlot {
            tag: TAG_INT,
            payload: *value as u64,
        },
        Value::UInt(value) => JitSlot {
            tag: TAG_UINT,
            payload: *value,
        },
        Value::Float(value) => JitSlot {
            tag: TAG_FLOAT,
            payload: value.to_bits(),
        },
        Value::Bool(value) => JitSlot {
            tag: TAG_BOOL,
            payload: u64::from(*value),
        },
        Value::Char(value) => JitSlot {
            tag: TAG_CHAR,
            payload: *value as u32 as u64,
        },
        Value::Nil => JitSlot {
            tag: TAG_NIL,
            payload: 0,
        },
        _ => return None,
    })
}

fn jump_to(
    builder: &mut FunctionBuilder<'_>,
    target: Option<cranelift_codegen::ir::Block>,
    context: &'static str,
) -> Result<(), String> {
    let target = target.ok_or(context)?;
    builder.ins().jump(target, &[]);
    Ok(())
}

fn slot_address(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    stack: bool,
    index: usize,
) -> cranelift_codegen::ir::Value {
    let offset = if stack {
        offset_of!(JitFrame, stack)
    } else {
        offset_of!(JitFrame, locals)
    };
    let base = load_frame(builder, frame, offset as i32, types::I64);
    let byte_offset = index.saturating_mul(size_of::<JitSlot>()) as i64;
    builder.ins().iadd_imm_s(base, byte_offset)
}

fn load_slot_field(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    stack: bool,
    index: usize,
    field: i32,
) -> cranelift_codegen::ir::Value {
    let base = slot_address(builder, frame, stack, index);
    let address = if field == 0 {
        base
    } else {
        builder.ins().iadd_imm_s(base, i64::from(field))
    };
    builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), address, 0)
}

fn store_slot_field(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    stack: bool,
    index: usize,
    field: i32,
    value: cranelift_codegen::ir::Value,
) {
    let base = slot_address(builder, frame, stack, index);
    let address = if field == 0 {
        base
    } else {
        builder.ins().iadd_imm_s(base, i64::from(field))
    };
    builder
        .ins()
        .store(MemFlagsData::trusted(), value, address, 0);
}

fn store_slot_const(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    stack: bool,
    index: usize,
    slot: JitSlot,
) {
    let tag = builder.ins().iconst(types::I64, slot.tag as i64);
    let payload = builder.ins().iconst(types::I64, slot.payload as i64);
    store_slot_field(builder, frame, stack, index, 0, tag);
    store_slot_field(builder, frame, stack, index, 8, payload);
}

fn copy_slot(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    source_stack: bool,
    source: usize,
    target_stack: bool,
    target: usize,
) {
    let tag = load_slot_field(builder, frame, source_stack, source, 0);
    let payload = load_slot_field(builder, frame, source_stack, source, 8);
    store_slot_field(builder, frame, target_stack, target, 0, tag);
    store_slot_field(builder, frame, target_stack, target, 8, payload);
}

fn load_frame(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    offset: i32,
    ty: cranelift_codegen::ir::Type,
) -> cranelift_codegen::ir::Value {
    let address = builder.ins().iadd_imm_s(frame, i64::from(offset));
    builder.ins().load(ty, MemFlagsData::trusted(), address, 0)
}

fn store_frame_i64(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    offset: i32,
    value: i64,
) {
    let value = builder.ins().iconst(types::I64, value);
    let address = builder.ins().iadd_imm_s(frame, i64::from(offset));
    builder
        .ins()
        .store(MemFlagsData::trusted(), value, address, 0);
}

fn set_stack_len(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    depth: usize,
) {
    store_frame_i64(
        builder,
        frame,
        offset_of!(JitFrame, stack_len) as i32,
        depth as i64,
    );
}

fn emit_exit(
    builder: &mut FunctionBuilder<'_>,
    _frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    stack_depth: usize,
    status: i32,
    reason: i64,
) {
    let exit_reason = builder.ins().iconst(types::I64, reason);
    let result = builder.ins().iconst(types::I32, i64::from(status));
    let pc = builder.ins().iconst(types::I64, pc as i64);
    let stack_depth = builder.ins().iconst(types::I64, stack_depth as i64);
    builder.ins().jump(
        local_state.exit_block,
        &[
            BlockArg::Value(result),
            BlockArg::Value(exit_reason),
            BlockArg::Value(pc),
            BlockArg::Value(stack_depth),
        ],
    );
}

fn emit_numeric_binary(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
) {
    let left_tag = load_slot_field(builder, frame, true, depth - 2, 0);
    let right_tag = load_slot_field(builder, frame, true, depth - 1, 0);
    let left_is_int = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_INT as i64);
    let right_is_int = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_INT as i64);
    let both_int = builder.ins().band(left_is_int, right_is_int);
    let left_is_uint = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_UINT as i64);
    let right_is_uint = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_UINT as i64);
    let both_uint = builder.ins().band(left_is_uint, right_is_uint);
    let left_is_float = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_FLOAT as i64);
    let right_is_float = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_FLOAT as i64);
    let both_float = builder.ins().band(left_is_float, right_is_float);
    let signed = builder.create_block();
    let check_unsigned = builder.create_block();
    let unsigned = builder.create_block();
    let check_float = builder.create_block();
    let float = (!matches!(instruction, Instruction::Mod)).then(|| builder.create_block());
    let fallback = builder.create_block();
    let bigint_check = matches!(
        instruction,
        Instruction::Add | Instruction::Sub | Instruction::Mul
    )
    .then(|| builder.create_block());
    builder
        .ins()
        .brif(both_int, signed, &[], check_unsigned, &[]);
    builder.switch_to_block(check_unsigned);
    builder
        .ins()
        .brif(both_uint, unsigned, &[], check_float, &[]);
    builder.switch_to_block(check_float);
    if let Some(float) = float {
        builder.ins().brif(
            both_float,
            float,
            &[],
            bigint_check.unwrap_or(fallback),
            &[],
        );
    } else {
        emit_exit(
            builder,
            frame,
            local_state,
            pc,
            depth,
            EXIT_RESUME,
            REASON_NORMAL,
        );
    }
    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );
    builder.switch_to_block(signed);
    emit_integer_binary_kind(
        builder,
        frame,
        local_state,
        pc,
        depth,
        next,
        instruction,
        TAG_INT,
        true,
    );
    builder.switch_to_block(unsigned);
    emit_integer_binary_kind(
        builder,
        frame,
        local_state,
        pc,
        depth,
        next,
        instruction,
        TAG_UINT,
        false,
    );
    if let Some(float) = float {
        builder.switch_to_block(float);
        emit_float_binary(builder, frame, local_state, pc, depth, next, instruction);
    }
    if let Some(bigint_check) = bigint_check {
        builder.switch_to_block(bigint_check);
        let left_bigint = small_bigint_tag_test(builder, left_tag);
        let right_bigint = small_bigint_tag_test(builder, right_tag);
        let both_bigint = builder.ins().band(left_bigint, right_bigint);
        let bigint = builder.create_block();
        builder.ins().brif(both_bigint, bigint, &[], fallback, &[]);
        builder.switch_to_block(bigint);
        emit_bigint_binary(
            builder,
            frame,
            local_state,
            pc,
            depth,
            next,
            instruction,
            left_tag,
            right_tag,
        );
    }
}

fn emit_bigint_binary(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
    left_tag: cranelift_codegen::ir::Value,
    right_tag: cranelift_codegen::ir::Value,
) {
    let left_signed = small_bigint_signed_tag_test(builder, left_tag);
    let right_signed = small_bigint_signed_tag_test(builder, right_tag);
    let both_signed = builder.ins().band(left_signed, right_signed);
    let left = load_slot_field(builder, frame, true, depth - 2, 8);
    let right = load_slot_field(builder, frame, true, depth - 1, 8);
    let fallback = builder.create_block();
    let signed = builder.create_block();
    let check_unsigned = builder.create_block();
    let unsigned = builder.create_block();
    builder
        .ins()
        .brif(both_signed, signed, &[], check_unsigned, &[]);

    builder.switch_to_block(check_unsigned);
    let left_unsigned = small_bigint_unsigned_tag_test(builder, left_tag);
    let right_unsigned = small_bigint_unsigned_tag_test(builder, right_tag);
    let both_unsigned = builder.ins().band(left_unsigned, right_unsigned);
    builder
        .ins()
        .brif(both_unsigned, unsigned, &[], fallback, &[]);

    builder.switch_to_block(signed);
    let (result, overflow) = match instruction {
        Instruction::Add => {
            let result = builder.ins().iadd(left, right);
            let left_sign = builder.ins().bxor(left, result);
            let right_sign = builder.ins().bxor(right, result);
            let signs = builder.ins().band(left_sign, right_sign);
            let overflow = builder.ins().icmp_imm_s(IntCC::SignedLessThan, signs, 0);
            (result, overflow)
        }
        Instruction::Sub => {
            let operands = builder.ins().bxor(left, right);
            let result = builder.ins().isub(left, right);
            let left_sign = builder.ins().bxor(left, result);
            let signs = builder.ins().band(operands, left_sign);
            let overflow = builder.ins().icmp_imm_s(IntCC::SignedLessThan, signs, 0);
            (result, overflow)
        }
        Instruction::Mul => builder.ins().smul_overflow(left, right),
        _ => unreachable!("BigInt fast path handles add, sub, and mul"),
    };
    let safe = builder.create_block();
    builder.ins().brif(overflow, fallback, &[], safe, &[]);
    builder.switch_to_block(safe);
    emit_bigint_result(builder, frame, depth, next, result, TAG_BIGINT_JIT_SIGNED);

    builder.switch_to_block(unsigned);
    let (result, overflow) = match instruction {
        Instruction::Add => builder.ins().uadd_overflow(left, right),
        Instruction::Sub => builder.ins().usub_overflow(left, right),
        Instruction::Mul => builder.ins().umul_overflow(left, right),
        _ => unreachable!("BigInt fast path handles add, sub, and mul"),
    };
    if matches!(instruction, Instruction::Sub) {
        let no_underflow = builder.ins().icmp_imm_s(IntCC::Equal, overflow, 0);
        let result_is_negative = builder.ins().icmp_imm_s(IntCC::SignedLessThan, result, 0);
        let negative_fits = builder.ins().band(overflow, result_is_negative);
        let fits = builder.ins().bor(no_underflow, negative_fits);
        let safe = builder.create_block();
        builder.ins().brif(fits, safe, &[], fallback, &[]);
        builder.switch_to_block(safe);
        let signed_tag = builder
            .ins()
            .iconst(types::I64, TAG_BIGINT_JIT_SIGNED as i64);
        let unsigned_tag = builder
            .ins()
            .iconst(types::I64, TAG_BIGINT_JIT_UNSIGNED as i64);
        let result_tag = builder
            .ins()
            .select(negative_fits, signed_tag, unsigned_tag);
        emit_bigint_dynamic_result(builder, frame, depth, next, result, result_tag);
    } else {
        let safe = builder.create_block();
        builder.ins().brif(overflow, fallback, &[], safe, &[]);
        builder.switch_to_block(safe);
        emit_bigint_result(builder, frame, depth, next, result, TAG_BIGINT_JIT_UNSIGNED);
    }

    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );
}

fn emit_bigint_result(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    result: cranelift_codegen::ir::Value,
    result_tag: u64,
) {
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: result_tag,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, result);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

fn emit_bigint_dynamic_result(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    result: cranelift_codegen::ir::Value,
    result_tag: cranelift_codegen::ir::Value,
) {
    store_slot_field(builder, frame, true, depth - 2, 0, result_tag);
    store_slot_field(builder, frame, true, depth - 2, 8, result);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

fn emit_float_binary(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
) {
    let left_bits = load_slot_field(builder, frame, true, depth - 2, 8);
    let right_bits = load_slot_field(builder, frame, true, depth - 1, 8);
    let left = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), left_bits);
    let right = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), right_bits);
    let result = match instruction {
        Instruction::Add => builder.ins().fadd(left, right),
        Instruction::Sub => builder.ins().fsub(left, right),
        Instruction::Mul => builder.ins().fmul(left, right),
        Instruction::Div => {
            let zero = builder.ins().f64const(Ieee64::with_bits(0));
            let is_zero = builder.ins().fcmp(FloatCC::Equal, right, zero);
            let fail = builder.create_block();
            let safe = builder.create_block();
            builder.ins().brif(is_zero, fail, &[], safe, &[]);
            builder.switch_to_block(fail);
            emit_exit(
                builder,
                frame,
                local_state,
                pc,
                depth,
                EXIT_FAULT,
                REASON_NORMAL,
            );
            builder.switch_to_block(safe);
            builder.ins().fdiv(left, right)
        }
        _ => unreachable!("float lowering supports add, sub, mul, and div"),
    };
    let result = builder
        .ins()
        .bitcast(types::I64, MemFlagsData::new(), result);
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: TAG_FLOAT,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, result);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

fn emit_integer_binary_kind(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
    result_tag: u64,
    signed: bool,
) {
    let left = load_slot_field(builder, frame, true, depth - 2, 8);
    let right = load_slot_field(builder, frame, true, depth - 1, 8);

    let (result, overflow) = match instruction {
        Instruction::Add => {
            let (result, overflow) = if signed {
                let result = builder.ins().iadd(left, right);
                let lhs_sign = builder.ins().bxor(left, result);
                let rhs_sign = builder.ins().bxor(right, result);
                let signs = builder.ins().band(lhs_sign, rhs_sign);
                let overflow = builder.ins().icmp_imm_s(IntCC::SignedLessThan, signs, 0);
                (result, overflow)
            } else {
                builder.ins().uadd_overflow(left, right)
            };
            (result, Some(overflow))
        }
        Instruction::Sub => {
            let (result, overflow) = if signed {
                let operands = builder.ins().bxor(left, right);
                let result = builder.ins().isub(left, right);
                let lhs_sign = builder.ins().bxor(left, result);
                let signs = builder.ins().band(operands, lhs_sign);
                let overflow = builder.ins().icmp_imm_s(IntCC::SignedLessThan, signs, 0);
                (result, overflow)
            } else {
                builder.ins().usub_overflow(left, right)
            };
            (result, Some(overflow))
        }
        Instruction::Mul if signed => {
            let (result, overflow) = builder.ins().smul_overflow(left, right);
            (result, Some(overflow))
        }
        Instruction::Mul => {
            let (result, overflow) = builder.ins().umul_overflow(left, right);
            (result, Some(overflow))
        }
        Instruction::Div | Instruction::Mod => {
            let zero = builder.ins().icmp_imm_s(IntCC::Equal, right, 0);
            let overflow = if signed {
                let min = builder.ins().icmp_imm_s(IntCC::Equal, left, i64::MIN);
                let minus_one = builder.ins().icmp_imm_s(IntCC::Equal, right, -1);
                let min_overflow = builder.ins().band(min, minus_one);
                builder.ins().bor(zero, min_overflow)
            } else {
                zero
            };
            let fail = builder.create_block();
            let safe = builder.create_block();
            builder.ins().brif(overflow, fail, &[], safe, &[]);
            builder.switch_to_block(fail);
            emit_exit(
                builder,
                frame,
                local_state,
                pc,
                depth,
                EXIT_FAULT,
                REASON_NORMAL,
            );
            builder.switch_to_block(safe);
            let result = match (instruction, signed) {
                (Instruction::Div, true) => builder.ins().sdiv(left, right),
                (Instruction::Div, false) => builder.ins().udiv(left, right),
                (Instruction::Mod, true) => builder.ins().srem(left, right),
                (Instruction::Mod, false) => builder.ins().urem(left, right),
                _ => unreachable!("integer division lowering only handles div and mod"),
            };
            store_slot_const(
                builder,
                frame,
                true,
                depth - 2,
                JitSlot {
                    tag: result_tag,
                    payload: 0,
                },
            );
            store_slot_field(builder, frame, true, depth - 2, 8, result);
            set_stack_len(builder, frame, depth - 1);
            builder.ins().jump(next, &[]);
            return;
        }
        _ => unreachable!("integer lowering is used only for arithmetic"),
    };
    let fail = builder.create_block();
    let safe = builder.create_block();
    builder.ins().brif(
        overflow.expect("checked arithmetic overflow flag"),
        fail,
        &[],
        safe,
        &[],
    );
    builder.switch_to_block(fail);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_FAULT,
        REASON_NORMAL,
    );
    builder.switch_to_block(safe);
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: result_tag,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, result);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

fn tag_range_test(
    builder: &mut FunctionBuilder<'_>,
    tag: cranelift_codegen::ir::Value,
    base: u64,
) -> cranelift_codegen::ir::Value {
    let lower = builder
        .ins()
        .icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, tag, base as i64);
    let upper = builder.ins().icmp_imm_s(
        IntCC::UnsignedLessThan,
        tag,
        base.saturating_add(MAX_FRAME_ROOTS as u64) as i64,
    );
    builder.ins().band(lower, upper)
}

fn small_bigint_signed_tag_test(
    builder: &mut FunctionBuilder<'_>,
    tag: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let rooted = tag_range_test(builder, tag, TAG_BIGINT_SMALL_SIGNED_BASE);
    let computed = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, tag, TAG_BIGINT_JIT_SIGNED as i64);
    builder.ins().bor(rooted, computed)
}

fn small_bigint_unsigned_tag_test(
    builder: &mut FunctionBuilder<'_>,
    tag: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let rooted = tag_range_test(builder, tag, TAG_BIGINT_SMALL_UNSIGNED_BASE);
    let computed = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, tag, TAG_BIGINT_JIT_UNSIGNED as i64);
    builder.ins().bor(rooted, computed)
}

fn small_bigint_tag_test(
    builder: &mut FunctionBuilder<'_>,
    tag: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let signed = small_bigint_signed_tag_test(builder, tag);
    let unsigned = small_bigint_unsigned_tag_test(builder, tag);
    builder.ins().bor(signed, unsigned)
}

fn emit_integer_comparison(
    builder: &mut FunctionBuilder<'_>,
    instruction: &Instruction,
    left: cranelift_codegen::ir::Value,
    right: cranelift_codegen::ir::Value,
    left_signed: cranelift_codegen::ir::Value,
    right_signed: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let signed_less = builder.ins().icmp(IntCC::SignedLessThan, left, right);
    let signed_less_equal = builder
        .ins()
        .icmp(IntCC::SignedLessThanOrEqual, left, right);
    let signed_greater = builder.ins().icmp(IntCC::SignedGreaterThan, left, right);
    let signed_greater_equal = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, left, right);
    let unsigned_less = builder.ins().icmp(IntCC::UnsignedLessThan, left, right);
    let unsigned_less_equal = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, left, right);
    let unsigned_greater = builder.ins().icmp(IntCC::UnsignedGreaterThan, left, right);
    let unsigned_greater_equal = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, left, right);
    let equal = builder.ins().icmp(IntCC::Equal, left, right);
    let not_equal = builder.ins().icmp(IntCC::NotEqual, left, right);
    let zero = builder.ins().iconst(types::I64, 0);
    let left_negative = builder.ins().icmp(IntCC::SignedLessThan, left, zero);
    let right_negative = builder.ins().icmp(IntCC::SignedLessThan, right, zero);
    let left_nonnegative = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, left, zero);
    let right_nonnegative = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, right, zero);
    let true_value = builder.ins().iconst(types::I8, 1);
    let false_value = builder.ins().iconst(types::I8, 0);

    let same_signedness = match instruction {
        Instruction::Eq => equal,
        Instruction::Ne => not_equal,
        Instruction::Lt => builder
            .ins()
            .select(left_signed, signed_less, unsigned_less),
        Instruction::Le => {
            builder
                .ins()
                .select(left_signed, signed_less_equal, unsigned_less_equal)
        }
        Instruction::Gt => builder
            .ins()
            .select(left_signed, signed_greater, unsigned_greater),
        Instruction::Ge => {
            builder
                .ins()
                .select(left_signed, signed_greater_equal, unsigned_greater_equal)
        }
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };

    let signed_left = match instruction {
        Instruction::Eq => builder.ins().band(left_nonnegative, equal),
        Instruction::Ne => builder.ins().bor(left_negative, not_equal),
        Instruction::Lt => builder
            .ins()
            .select(left_negative, true_value, unsigned_less),
        Instruction::Le => builder
            .ins()
            .select(left_negative, true_value, unsigned_less_equal),
        Instruction::Gt => builder
            .ins()
            .select(left_negative, false_value, unsigned_greater),
        Instruction::Ge => builder
            .ins()
            .select(left_negative, false_value, unsigned_greater_equal),
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };
    let unsigned_left = match instruction {
        Instruction::Eq => builder.ins().band(right_nonnegative, equal),
        Instruction::Ne => builder.ins().bor(right_negative, not_equal),
        Instruction::Lt => builder
            .ins()
            .select(right_negative, false_value, unsigned_less),
        Instruction::Le => builder
            .ins()
            .select(right_negative, false_value, unsigned_less_equal),
        Instruction::Gt => builder
            .ins()
            .select(right_negative, true_value, unsigned_greater),
        Instruction::Ge => builder
            .ins()
            .select(right_negative, true_value, unsigned_greater_equal),
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };
    let mixed = builder
        .ins()
        .icmp(IntCC::NotEqual, left_signed, right_signed);
    let mixed_result = builder
        .ins()
        .select(left_signed, signed_left, unsigned_left);
    builder.ins().select(mixed, mixed_result, same_signedness)
}

fn emit_numeric_compare(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
) {
    let left_tag = load_slot_field(builder, frame, true, depth - 2, 0);
    let right_tag = load_slot_field(builder, frame, true, depth - 1, 0);
    let left_is_int = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_INT as i64);
    let right_is_int = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_INT as i64);
    let both_int = builder.ins().band(left_is_int, right_is_int);
    let left_is_uint = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_UINT as i64);
    let right_is_uint = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_UINT as i64);
    let both_uint = builder.ins().band(left_is_uint, right_is_uint);
    let int_uint = builder.ins().band(left_is_int, right_is_uint);
    let uint_int = builder.ins().band(left_is_uint, right_is_int);
    let mixed_int_uint = builder.ins().bor(int_uint, uint_int);
    let left_is_float = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_FLOAT as i64);
    let right_is_float = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_FLOAT as i64);
    let both_float = builder.ins().band(left_is_float, right_is_float);
    let int_float = builder.ins().band(left_is_int, right_is_float);
    let float_int = builder.ins().band(left_is_float, right_is_int);
    let mixed_int_float = builder.ins().bor(int_float, float_int);
    let uint_float = builder.ins().band(left_is_uint, right_is_float);
    let float_uint = builder.ins().band(left_is_float, right_is_uint);
    let mixed_uint_float = builder.ins().bor(uint_float, float_uint);
    let left_is_nil = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_NIL as i64);
    let right_is_nil = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_NIL as i64);
    let left_is_char = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_CHAR as i64);
    let right_is_char = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_CHAR as i64);
    let both_char = builder.ins().band(left_is_char, right_is_char);
    let left_is_bool = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_BOOL as i64);
    let right_is_bool = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_BOOL as i64);
    let both_bool = builder.ins().band(left_is_bool, right_is_bool);
    let bool_equality = matches!(instruction, Instruction::Eq | Instruction::Ne);
    let both_comparable_bool = if bool_equality {
        both_bool
    } else {
        builder.ins().iconst(types::I8, 0)
    };
    let integer = builder.ins().bor(both_int, both_uint);
    let integer = builder.ins().bor(integer, both_char);
    let integer = builder.ins().bor(integer, both_comparable_bool);
    let base_valid = builder.ins().bor(integer, both_float);
    let base_valid = builder.ins().bor(base_valid, mixed_int_uint);
    let mixed_float = builder.ins().bor(mixed_int_float, mixed_uint_float);
    let check_mixed = builder.create_block();
    let check_nil = builder.create_block();
    let mixed_float_block = builder.create_block();
    let regular_block = builder.create_block();
    let nil_block = builder.create_block();
    let nil_valid_block = builder.create_block();
    let check_bigint = builder.create_block();
    let bigint_block = builder.create_block();
    let fallback = builder.create_block();
    builder
        .ins()
        .brif(base_valid, regular_block, &[], check_mixed, &[]);

    builder.switch_to_block(check_mixed);
    builder
        .ins()
        .brif(mixed_float, mixed_float_block, &[], check_nil, &[]);

    builder.switch_to_block(check_nil);
    let nil_pair = if bool_equality {
        builder.ins().bor(left_is_nil, right_is_nil)
    } else {
        builder.ins().iconst(types::I8, 0)
    };
    builder
        .ins()
        .brif(nil_pair, nil_block, &[], check_bigint, &[]);

    builder.switch_to_block(check_bigint);
    let left_small_bigint = small_bigint_tag_test(builder, left_tag);
    let right_small_bigint = small_bigint_tag_test(builder, right_tag);
    let left_numeric = builder.ins().bor(left_is_int, left_is_uint);
    let left_numeric = builder.ins().bor(left_numeric, left_is_float);
    let right_numeric = builder.ins().bor(right_is_int, right_is_uint);
    let right_numeric = builder.ins().bor(right_numeric, right_is_float);
    let left_bigint_pair = builder.ins().band(left_small_bigint, right_numeric);
    let right_bigint_pair = builder.ins().band(right_small_bigint, left_numeric);
    let bigint_pair = builder.ins().bor(left_bigint_pair, right_bigint_pair);
    builder
        .ins()
        .brif(bigint_pair, bigint_block, &[], fallback, &[]);

    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );

    builder.switch_to_block(nil_block);
    let left_uninitialized =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, left_tag, TAG_UNINITIALIZED as i64);
    let right_uninitialized =
        builder
            .ins()
            .icmp_imm_s(IntCC::Equal, right_tag, TAG_UNINITIALIZED as i64);
    let uninitialized = builder.ins().bor(left_uninitialized, right_uninitialized);
    builder
        .ins()
        .brif(uninitialized, fallback, &[], nil_valid_block, &[]);

    builder.switch_to_block(nil_valid_block);
    let both_nil = builder.ins().band(left_is_nil, right_is_nil);
    let result = if matches!(instruction, Instruction::Eq) {
        both_nil
    } else {
        builder.ins().icmp_imm_s(IntCC::Equal, both_nil, 0)
    };
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(bigint_block);
    let left_payload = load_slot_field(builder, frame, true, depth - 2, 8);
    let right_payload = load_slot_field(builder, frame, true, depth - 1, 8);
    let has_float = builder.ins().bor(left_is_float, right_is_float);
    let bigint_float_block = builder.create_block();
    let bigint_integer_block = builder.create_block();
    builder.ins().brif(
        has_float,
        bigint_float_block,
        &[],
        bigint_integer_block,
        &[],
    );

    builder.switch_to_block(bigint_float_block);
    let integer_payload = builder
        .ins()
        .select(left_is_float, right_payload, left_payload);
    let float_payload = builder
        .ins()
        .select(left_is_float, left_payload, right_payload);
    let float_value = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), float_payload);
    let left_small_signed = small_bigint_signed_tag_test(builder, left_tag);
    let right_small_signed = small_bigint_signed_tag_test(builder, right_tag);
    let left_signed = builder.ins().bor(left_is_int, left_small_signed);
    let right_signed = builder.ins().bor(right_is_int, right_small_signed);
    let is_signed = builder.ins().bor(left_signed, right_signed);
    let signed_block = builder.create_block();
    let unsigned_block = builder.create_block();
    builder
        .ins()
        .brif(is_signed, signed_block, &[], unsigned_block, &[]);

    builder.switch_to_block(signed_block);
    let (less, equal, greater) =
        emit_integer_float_ordering(builder, integer_payload, float_value, true);
    let result = emit_order_predicate(builder, instruction, less, equal, greater, left_is_float);
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(unsigned_block);
    let (less, equal, greater) =
        emit_integer_float_ordering(builder, integer_payload, float_value, false);
    let result = emit_order_predicate(builder, instruction, less, equal, greater, left_is_float);
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(bigint_integer_block);
    let left_small_signed = small_bigint_signed_tag_test(builder, left_tag);
    let right_small_signed = small_bigint_signed_tag_test(builder, right_tag);
    let left_signed = builder.ins().bor(left_is_int, left_small_signed);
    let right_signed = builder.ins().bor(right_is_int, right_small_signed);
    let result = emit_integer_comparison(
        builder,
        instruction,
        left_payload,
        right_payload,
        left_signed,
        right_signed,
    );
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(mixed_float_block);
    let left_payload = load_slot_field(builder, frame, true, depth - 2, 8);
    let right_payload = load_slot_field(builder, frame, true, depth - 1, 8);
    let integer_payload = builder
        .ins()
        .select(left_is_float, right_payload, left_payload);
    let float_payload = builder
        .ins()
        .select(left_is_float, left_payload, right_payload);
    let float_value = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), float_payload);
    let is_signed = builder.ins().bor(left_is_int, right_is_int);
    let signed_block = builder.create_block();
    let unsigned_block = builder.create_block();
    builder
        .ins()
        .brif(is_signed, signed_block, &[], unsigned_block, &[]);

    builder.switch_to_block(signed_block);
    let (less, equal, greater) =
        emit_integer_float_ordering(builder, integer_payload, float_value, true);
    let result = emit_order_predicate(builder, instruction, less, equal, greater, left_is_float);
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(unsigned_block);
    let (less, equal, greater) =
        emit_integer_float_ordering(builder, integer_payload, float_value, false);
    let result = emit_order_predicate(builder, instruction, less, equal, greater, left_is_float);
    emit_compare_result(builder, frame, depth, next, result);

    builder.switch_to_block(regular_block);
    let left = load_slot_field(builder, frame, true, depth - 2, 8);
    let right = load_slot_field(builder, frame, true, depth - 1, 8);
    let integer_condition = match instruction {
        Instruction::Eq => builder.ins().icmp(IntCC::Equal, left, right),
        Instruction::Ne => builder.ins().icmp(IntCC::NotEqual, left, right),
        Instruction::Lt => {
            let signed = builder.ins().icmp(IntCC::SignedLessThan, left, right);
            let unsigned = builder.ins().icmp(IntCC::UnsignedLessThan, left, right);
            let unsigned_type = builder.ins().bor(both_uint, both_char);
            builder.ins().select(unsigned_type, unsigned, signed)
        }
        Instruction::Le => {
            let signed = builder
                .ins()
                .icmp(IntCC::SignedLessThanOrEqual, left, right);
            let unsigned = builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, left, right);
            let unsigned_type = builder.ins().bor(both_uint, both_char);
            builder.ins().select(unsigned_type, unsigned, signed)
        }
        Instruction::Gt => {
            let signed = builder.ins().icmp(IntCC::SignedGreaterThan, left, right);
            let unsigned = builder.ins().icmp(IntCC::UnsignedGreaterThan, left, right);
            let unsigned_type = builder.ins().bor(both_uint, both_char);
            builder.ins().select(unsigned_type, unsigned, signed)
        }
        Instruction::Ge => {
            let signed = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, left, right);
            let unsigned = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, left, right);
            let unsigned_type = builder.ins().bor(both_uint, both_char);
            builder.ins().select(unsigned_type, unsigned, signed)
        }
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };
    let zero = builder.ins().iconst(types::I64, 0);
    let left_negative = builder.ins().icmp(IntCC::SignedLessThan, left, zero);
    let right_negative = builder.ins().icmp(IntCC::SignedLessThan, right, zero);
    let equal = builder.ins().icmp(IntCC::Equal, left, right);
    let not_equal = builder.ins().icmp(IntCC::NotEqual, left, right);
    let less = builder.ins().icmp(IntCC::UnsignedLessThan, left, right);
    let less_equal = builder
        .ins()
        .icmp(IntCC::UnsignedLessThanOrEqual, left, right);
    let greater = builder.ins().icmp(IntCC::UnsignedGreaterThan, left, right);
    let greater_equal = builder
        .ins()
        .icmp(IntCC::UnsignedGreaterThanOrEqual, left, right);
    let true_value = builder.ins().iconst(types::I8, 1);
    let false_value = builder.ins().iconst(types::I8, 0);
    let left_nonnegative = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, left, zero);
    let right_nonnegative = builder
        .ins()
        .icmp(IntCC::SignedGreaterThanOrEqual, right, zero);
    let int_uint_condition = match instruction {
        Instruction::Eq => builder.ins().band(left_nonnegative, equal),
        Instruction::Ne => builder.ins().bor(left_negative, not_equal),
        Instruction::Lt => builder.ins().select(left_negative, true_value, less),
        Instruction::Le => builder.ins().select(left_negative, true_value, less_equal),
        Instruction::Gt => builder.ins().select(left_negative, false_value, greater),
        Instruction::Ge => builder
            .ins()
            .select(left_negative, false_value, greater_equal),
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };
    let uint_int_condition = match instruction {
        Instruction::Eq => builder.ins().band(right_nonnegative, equal),
        Instruction::Ne => builder.ins().bor(right_negative, not_equal),
        Instruction::Lt => builder.ins().select(right_negative, false_value, less),
        Instruction::Le => builder
            .ins()
            .select(right_negative, false_value, less_equal),
        Instruction::Gt => builder.ins().select(right_negative, true_value, greater),
        Instruction::Ge => builder
            .ins()
            .select(right_negative, true_value, greater_equal),
        _ => unreachable!("integer comparison lowering handles only comparisons"),
    };
    let mixed_condition = builder
        .ins()
        .select(int_uint, int_uint_condition, uint_int_condition);
    let integer_condition =
        builder
            .ins()
            .select(mixed_int_uint, mixed_condition, integer_condition);
    let left_float = builder.ins().bitcast(types::F64, MemFlagsData::new(), left);
    let right_float = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), right);
    let float_condition = match instruction {
        Instruction::Eq => builder.ins().fcmp(FloatCC::Equal, left_float, right_float),
        Instruction::Ne => builder
            .ins()
            .fcmp(FloatCC::NotEqual, left_float, right_float),
        Instruction::Lt => builder
            .ins()
            .fcmp(FloatCC::LessThan, left_float, right_float),
        Instruction::Le => builder
            .ins()
            .fcmp(FloatCC::LessThanOrEqual, left_float, right_float),
        Instruction::Gt => builder
            .ins()
            .fcmp(FloatCC::GreaterThan, left_float, right_float),
        Instruction::Ge => builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, left_float, right_float),
        _ => unreachable!("float comparison lowering handles only comparisons"),
    };
    let condition = builder
        .ins()
        .select(both_float, float_condition, integer_condition);
    let payload = builder.ins().uextend(types::I64, condition);
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: TAG_BOOL,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, payload);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

/// Writes a boolean comparison result and consumes the two input stack slots.
fn emit_compare_result(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    result: cranelift_codegen::ir::Value,
) {
    let payload = builder.ins().uextend(types::I64, result);
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: TAG_BOOL,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, payload);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

/// Mirrors `compare_int_float` and `compare_uint_float` in `value.rs` without
/// rounding the integer to f64. Saturating conversions are safe for every f64;
/// the in-range predicates ensure their saturated values only affect the
/// fallback ordering that is discarded for out-of-range inputs.
fn emit_integer_float_ordering(
    builder: &mut FunctionBuilder<'_>,
    integer: cranelift_codegen::ir::Value,
    float: cranelift_codegen::ir::Value,
    signed: bool,
) -> (
    cranelift_codegen::ir::Value,
    cranelift_codegen::ir::Value,
    cranelift_codegen::ir::Value,
) {
    let zero = builder.ins().f64const(Ieee64::with_bits(0));
    let ordered = builder.ins().fcmp(FloatCC::Equal, float, float);
    let (
        lower_bound,
        upper_bound,
        below,
        above,
        integer_less,
        integer_equal,
        integer_greater,
        truncated_float,
    ) = if signed {
        let lower_bits = Ieee64::with_bits(0xc3e0_0000_0000_0000);
        let upper_bits = Ieee64::with_bits(0x43e0_0000_0000_0000);
        let lower = builder.ins().f64const(lower_bits);
        let upper = builder.ins().f64const(upper_bits);
        let below = builder.ins().fcmp(FloatCC::LessThan, float, lower);
        let above = builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, float, upper);
        let truncated = builder.ins().fcvt_to_sint_sat(types::I64, float);
        let integer_less = builder
            .ins()
            .icmp(IntCC::SignedLessThan, integer, truncated);
        let integer_equal = builder.ins().icmp(IntCC::Equal, integer, truncated);
        let integer_greater = builder
            .ins()
            .icmp(IntCC::SignedGreaterThan, integer, truncated);
        let truncated_float = builder.ins().fcvt_from_sint(types::F64, truncated);
        (
            lower,
            upper,
            below,
            above,
            integer_less,
            integer_equal,
            integer_greater,
            truncated_float,
        )
    } else {
        let lower = zero;
        let upper = builder
            .ins()
            .f64const(Ieee64::with_bits(0x43f0_0000_0000_0000));
        let below = builder.ins().fcmp(FloatCC::LessThan, float, lower);
        let above = builder
            .ins()
            .fcmp(FloatCC::GreaterThanOrEqual, float, upper);
        let truncated = builder.ins().fcvt_to_uint_sat(types::I64, float);
        let integer_less = builder
            .ins()
            .icmp(IntCC::UnsignedLessThan, integer, truncated);
        let integer_equal = builder.ins().icmp(IntCC::Equal, integer, truncated);
        let integer_greater = builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, integer, truncated);
        let truncated_float = builder.ins().fcvt_from_uint(types::F64, truncated);
        (
            lower,
            upper,
            below,
            above,
            integer_less,
            integer_equal,
            integer_greater,
            truncated_float,
        )
    };
    let in_range = builder
        .ins()
        .fcmp(FloatCC::GreaterThanOrEqual, float, lower_bound);
    let below_upper = builder.ins().fcmp(FloatCC::LessThan, float, upper_bound);
    let in_range = builder.ins().band(in_range, below_upper);
    let in_range = builder.ins().band(in_range, ordered);
    let truncated_less = builder
        .ins()
        .fcmp(FloatCC::LessThan, truncated_float, float);
    let truncated_equal = builder.ins().fcmp(FloatCC::Equal, truncated_float, float);
    let truncated_greater = builder
        .ins()
        .fcmp(FloatCC::GreaterThan, truncated_float, float);
    let inner_less = builder
        .ins()
        .select(integer_equal, truncated_less, integer_less);
    let inner_greater = builder
        .ins()
        .select(integer_equal, truncated_greater, integer_greater);
    let in_range_less = builder.ins().band(in_range, inner_less);
    let less = builder.ins().bor(below, in_range_less);
    let in_range_greater = builder.ins().band(in_range, inner_greater);
    let greater = builder.ins().bor(above, in_range_greater);
    let trunc_match = builder.ins().band(integer_equal, truncated_equal);
    let equal = builder.ins().band(in_range, trunc_match);
    (less, equal, greater)
}

fn emit_order_predicate(
    builder: &mut FunctionBuilder<'_>,
    instruction: &Instruction,
    mut less: cranelift_codegen::ir::Value,
    equal: cranelift_codegen::ir::Value,
    mut greater: cranelift_codegen::ir::Value,
    reverse: cranelift_codegen::ir::Value,
) -> cranelift_codegen::ir::Value {
    let swapped_less = builder.ins().select(reverse, greater, less);
    let swapped_greater = builder.ins().select(reverse, less, greater);
    less = swapped_less;
    greater = swapped_greater;
    match instruction {
        Instruction::Eq => equal,
        Instruction::Ne => builder.ins().icmp_imm_s(IntCC::Equal, equal, 0),
        Instruction::Lt => less,
        Instruction::Le => builder.ins().bor(less, equal),
        Instruction::Gt => greater,
        Instruction::Ge => builder.ins().bor(greater, equal),
        _ => unreachable!("ordering predicate accepts only comparisons"),
    }
}

fn emit_bool_binary(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    next: cranelift_codegen::ir::Block,
    instruction: &Instruction,
) {
    let left_tag = load_slot_field(builder, frame, true, depth - 2, 0);
    let right_tag = load_slot_field(builder, frame, true, depth - 1, 0);
    let left_bool = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, left_tag, TAG_BOOL as i64);
    let right_bool = builder
        .ins()
        .icmp_imm_s(IntCC::Equal, right_tag, TAG_BOOL as i64);
    let both_bool = builder.ins().band(left_bool, right_bool);
    let valid = builder.create_block();
    let fallback = builder.create_block();
    builder.ins().brif(both_bool, valid, &[], fallback, &[]);
    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );
    builder.switch_to_block(valid);
    let left = load_slot_field(builder, frame, true, depth - 2, 8);
    let right = load_slot_field(builder, frame, true, depth - 1, 8);
    let result = match instruction {
        Instruction::And => builder.ins().band(left, right),
        Instruction::Or => builder.ins().bor(left, right),
        Instruction::Xor => builder.ins().bxor(left, right),
        _ => unreachable!("emit_bool_binary is used only for boolean operations"),
    };
    store_slot_const(
        builder,
        frame,
        true,
        depth - 2,
        JitSlot {
            tag: TAG_BOOL,
            payload: 0,
        },
    );
    store_slot_field(builder, frame, true, depth - 2, 8, result);
    set_stack_len(builder, frame, depth - 1);
    builder.ins().jump(next, &[]);
}

fn emit_jump(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    target: usize,
    target_depth: usize,
    target_block: cranelift_codegen::ir::Block,
) {
    if target > pc {
        set_stack_len(builder, frame, target_depth);
        builder.ins().jump(target_block, &[]);
        return;
    }

    let counter_address = builder
        .ins()
        .iadd_imm_s(frame, offset_of!(JitFrame, backedge_count) as i64);
    let count = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), counter_address, 0);
    let next_count = builder.ins().iadd_imm_s(count, 1);
    builder
        .ins()
        .store(MemFlagsData::trusted(), next_count, counter_address, 0);

    let budget = load_frame(
        builder,
        frame,
        offset_of!(JitFrame, backedge_budget) as i32,
        types::I64,
    );
    let bounded = builder.ins().icmp_imm_s(IntCC::NotEqual, budget, 0);
    let profile_path = builder.create_block();
    let unbounded_path = builder.create_block();
    builder
        .ins()
        .brif(bounded, profile_path, &[], unbounded_path, &[]);
    builder.switch_to_block(unbounded_path);
    set_stack_len(builder, frame, target_depth);
    builder.ins().jump(target_block, &[]);

    builder.switch_to_block(profile_path);
    let one = builder.ins().iconst(types::I64, 1);
    let remaining = builder.ins().isub(budget, one);
    let budget_address = builder
        .ins()
        .iadd_imm_s(frame, offset_of!(JitFrame, backedge_budget) as i64);
    builder
        .ins()
        .store(MemFlagsData::trusted(), remaining, budget_address, 0);
    let exhausted = builder.ins().icmp_imm_s(IntCC::Equal, remaining, 0);
    let slice_end = builder.create_block();
    let keep_running = builder.create_block();
    builder
        .ins()
        .brif(exhausted, slice_end, &[], keep_running, &[]);
    builder.switch_to_block(slice_end);
    emit_exit(
        builder,
        frame,
        local_state,
        target,
        target_depth,
        EXIT_RESUME,
        REASON_PROFILE_SLICE,
    );
    builder.switch_to_block(keep_running);
    set_stack_len(builder, frame, target_depth);
    builder.ins().jump(target_block, &[]);
}

fn emit_conditional_jump(
    builder: &mut FunctionBuilder<'_>,
    frame: cranelift_codegen::ir::Value,
    local_state: &JitLocalState,
    pc: usize,
    depth: usize,
    target: usize,
    next: Option<cranelift_codegen::ir::Block>,
    blocks: &HashMap<usize, cranelift_codegen::ir::Block>,
    plan: &FunctionPlan,
    instruction: &Instruction,
) -> Result<(), String> {
    let next = next.ok_or("conditional fallthrough is missing")?;
    let tag = load_slot_field(builder, frame, true, depth - 1, 0);
    let is_bool = builder.ins().icmp_imm_s(IntCC::Equal, tag, TAG_BOOL as i64);
    let valid = builder.create_block();
    let fallback = builder.create_block();
    builder.ins().brif(is_bool, valid, &[], fallback, &[]);
    builder.switch_to_block(fallback);
    emit_exit(
        builder,
        frame,
        local_state,
        pc,
        depth,
        EXIT_RESUME,
        REASON_NORMAL,
    );

    builder.switch_to_block(valid);
    let payload = load_slot_field(builder, frame, true, depth - 1, 8);
    let take_target = match instruction {
        Instruction::JumpIfFalse(_) => builder.ins().icmp_imm_s(IntCC::Equal, payload, 0),
        Instruction::JumpIfTrue(_) => builder.ins().icmp_imm_s(IntCC::NotEqual, payload, 0),
        _ => unreachable!("conditional jump instruction expected"),
    };
    let target_block = blocks
        .get(&target)
        .copied()
        .ok_or("conditional target has no bytecode block")?;
    let taken = builder.create_block();
    let fallthrough = builder.create_block();
    builder
        .ins()
        .brif(take_target, taken, &[], fallthrough, &[]);

    builder.switch_to_block(taken);
    emit_jump(
        builder,
        frame,
        local_state,
        pc,
        target,
        plan.stack_depths[target - plan.entry].ok_or("invalid conditional stack depth")?,
        target_block,
    );

    builder.switch_to_block(fallthrough);
    let fall_depth = plan.stack_depths[pc + 1 - plan.entry]
        .ok_or("invalid conditional fallthrough stack depth")?;
    set_stack_len(builder, frame, fall_depth);
    builder.ins().jump(next, &[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_type_tests_use_registered_tags_and_fall_back_past_the_code_cap() {
        let artifact_with_variants =
            |count: usize| {
                let mut artifact = CompiledProgram::default();
                for index in 0..count {
                    artifact.instructions.push(Instruction::Push(Value::TypeTag(
                        std::rc::Rc::from(format!("Signal::V{index}")),
                    )));
                }
                artifact
                    .instructions
                    .push(Instruction::IsType(ValueType::Enum(std::rc::Rc::from(
                        "Signal",
                    ))));
                artifact
            };

        let supported = artifact_with_variants(MAX_JIT_ENUM_VARIANTS);
        let supported_tags = VariantTags::from_program(&supported);
        assert_eq!(
            supported_tags
                .variant_tags(&ValueType::Enum(std::rc::Rc::from("Signal")))
                .map(|tags| tags.len()),
            Some(MAX_JIT_ENUM_VARIANTS)
        );

        let too_large = artifact_with_variants(MAX_JIT_ENUM_VARIANTS + 1);
        let too_large_tags = VariantTags::from_program(&too_large);
        assert!(
            too_large_tags
                .variant_tags(&ValueType::Enum(std::rc::Rc::from("Signal")))
                .is_none()
        );
        assert!(
            VariantTags::default()
                .variant_tags(&ValueType::Variant(std::rc::Rc::from("External::Value")))
                .is_none()
        );
    }

    fn candidate(profile: FunctionProfile, windows_this_run: u32) -> Candidate {
        let now = Instant::now();
        Candidate {
            profile,
            window_edges: 0,
            windows_this_run,
            window_started: now,
            last_backedge_at: (windows_this_run > 0).then_some(now),
            last_selection_window: 0,
            retry_after_backedges: 0,
            compiled: None,
            disabled: false,
        }
    }

    #[test]
    fn adaptive_ape_uses_shared_calibration_without_stale_candidate_blocking() {
        let mut candidates = BTreeMap::new();
        // These calibrated records are historical: no candidate is active in
        // the current run, but their measurements remain useful to the model.
        for entry in 1..=MAX_BOOTSTRAP_PROBES {
            candidates.insert(
                entry,
                candidate(
                    FunctionProfile {
                        vm_ns_per_backedge: 10_000,
                        jit_ns_per_backedge: 5_000,
                        total_backedges: WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS),
                        hot_series_ns: MAX_HORIZON_NS,
                        active_windows: REQUIRED_WINDOWS,
                        calibrated: true,
                        ..FunctionProfile::default()
                    },
                    0,
                ),
            );
        }
        // This candidate has no private JIT measurements. Once bootstrap is
        // exhausted, it must use the conservative shared speedup estimate.
        candidates.insert(
            MAX_BOOTSTRAP_PROBES + 1,
            candidate(
                FunctionProfile {
                    vm_ns_per_backedge: 10_000,
                    total_backedges: WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS),
                    hot_series_ns: MAX_HORIZON_NS,
                    active_windows: REQUIRED_WINDOWS,
                    ..FunctionProfile::default()
                },
                REQUIRED_WINDOWS,
            ),
        );
        let entry = MAX_BOOTSTRAP_PROBES + 1;
        let plan = FunctionPlan {
            entry,
            end: entry + 16,
            instruction_count: 8,
            block_count: 2,
            edge_count: 1,
            phi_estimate: 0,
            local_count: 2,
            max_stack: 1,
            stack_depths: Vec::new(),
            osr_entries: vec![entry + 4],
        };
        let stale_entry = entry + 100;
        let mut stale_candidate = candidate(
            FunctionProfile {
                compile_ns: 1,
                vm_ns_per_backedge: 1_000_000,
                jit_ns_per_backedge: 1,
                total_backedges: WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS),
                hot_series_ns: MAX_HORIZON_NS,
                active_windows: REQUIRED_WINDOWS,
                calibrated: true,
            },
            REQUIRED_WINDOWS,
        );
        stale_candidate.last_backedge_at =
            Some(Instant::now() - std::time::Duration::from_nanos(MAX_ACTIVITY_GAP_NS + 1));
        let stale_plan = FunctionPlan {
            entry: stale_entry,
            end: stale_entry + (plan.end - plan.entry),
            osr_entries: vec![stale_entry + 4],
            ..plan.clone()
        };
        candidates.insert(stale_entry, stale_candidate);
        let (compiler, _) = JitCompiler::new().expect("native JIT should initialize");
        let mut state = JitState {
            plans: BTreeMap::from([(entry, Arc::new(plan)), (stale_entry, Arc::new(stale_plan))]),
            variant_tags: VariantTags::default(),
            candidates,
            compiler,
            coefficients: Coefficients::seeded(),
            profile_key: String::new(),
            profile_path: None,
            bootstrap_probes: MAX_BOOTSTRAP_PROBES,
            compile_tokens_ns: TOKEN_CAPACITY_NS as i64,
            token_refill_at: Instant::now(),
        };

        let decision = state.candidate_decision(entry);
        assert!(matches!(
            decision,
            ApeDecision::Compile { priority, .. } if priority > 0.0
        ));
        assert_eq!(state.best_candidate(Instant::now()), Some(entry));
        state
            .candidates
            .get_mut(&stale_entry)
            .expect("stale candidate is present")
            .last_backedge_at = Some(Instant::now());
        assert_eq!(state.best_candidate(Instant::now()), Some(stale_entry));
    }

    #[test]
    fn adaptive_ape_charges_estimated_compile_cost_before_first_probe() {
        let entry = 100;
        let mut candidates = BTreeMap::new();
        for calibrated_entry in 1..=MAX_BOOTSTRAP_PROBES {
            candidates.insert(
                calibrated_entry,
                candidate(
                    FunctionProfile {
                        vm_ns_per_backedge: 10_000,
                        jit_ns_per_backedge: 5_000,
                        active_windows: REQUIRED_WINDOWS,
                        calibrated: true,
                        ..FunctionProfile::default()
                    },
                    0,
                ),
            );
        }
        candidates.insert(
            entry,
            candidate(
                FunctionProfile {
                    vm_ns_per_backedge: 10_000,
                    total_backedges: WINDOW_BACKEDGES * u64::from(REQUIRED_WINDOWS),
                    hot_series_ns: 40_000,
                    active_windows: REQUIRED_WINDOWS,
                    ..FunctionProfile::default()
                },
                REQUIRED_WINDOWS,
            ),
        );
        let plan = FunctionPlan {
            entry,
            end: entry + 16,
            instruction_count: 8,
            block_count: 2,
            edge_count: 1,
            phi_estimate: 0,
            local_count: 2,
            max_stack: 1,
            stack_depths: Vec::new(),
            osr_entries: vec![entry + 4],
        };
        let (compiler, _) = JitCompiler::new().expect("native JIT should initialize");
        let state = JitState {
            plans: BTreeMap::from([(entry, Arc::new(plan))]),
            variant_tags: VariantTags::default(),
            candidates,
            compiler,
            coefficients: Coefficients::seeded(),
            profile_key: String::new(),
            profile_path: None,
            bootstrap_probes: MAX_BOOTSTRAP_PROBES,
            compile_tokens_ns: TOKEN_CAPACITY_NS as i64,
            token_refill_at: Instant::now(),
        };

        assert!(matches!(
            state.candidate_decision(entry),
            ApeDecision::Profile("not-profitable")
        ));
    }

    #[test]
    fn compile_budget_records_oversized_compilation_as_refillable_debt() {
        let mut available = TOKEN_CAPACITY_NS as i64;
        let reserved = reserve_compile_budget(&mut available, TOKEN_CAPACITY_NS * 2)
            .expect("a full bucket should admit one oversized compile");
        assert_eq!(reserved, TOKEN_CAPACITY_NS);
        assert_eq!(available, 0);

        settle_compile_budget(&mut available, reserved, TOKEN_CAPACITY_NS * 2);
        assert_eq!(available, -(TOKEN_CAPACITY_NS as i64));
        assert_eq!(
            reserve_compile_budget(&mut available, 1),
            Err(-(TOKEN_CAPACITY_NS as i64))
        );
    }

    #[test]
    fn run_local_heat_is_not_written_to_profiles() {
        let profile = FunctionProfile {
            compile_ns: 10,
            vm_ns_per_backedge: 20,
            jit_ns_per_backedge: 5,
            total_backedges: 900,
            hot_series_ns: MAX_HORIZON_NS,
            active_windows: REQUIRED_WINDOWS,
            calibrated: true,
        };
        let encoded = serde_json::to_value(profile).expect("profile serializes");
        assert!(encoded.get("total_backedges").is_none());
        assert!(encoded.get("hot_series_ns").is_none());
        assert!(encoded.get("active_windows").is_none());
        let restored: FunctionProfile =
            serde_json::from_value(encoded).expect("profile deserializes");
        assert_eq!(restored.total_backedges, 0);
        assert_eq!(restored.hot_series_ns, 0);
        assert_eq!(restored.active_windows, 0);
    }

    #[test]
    fn a_long_idle_gap_starts_a_new_hot_series() {
        let now = Instant::now();
        let mut candidate = candidate(
            FunctionProfile {
                total_backedges: 800,
                hot_series_ns: MAX_HORIZON_NS,
                active_windows: REQUIRED_WINDOWS,
                ..FunctionProfile::default()
            },
            REQUIRED_WINDOWS,
        );
        candidate.last_backedge_at =
            Some(now - std::time::Duration::from_nanos(MAX_ACTIVITY_GAP_NS + 1));

        assert!(!update_heat(&mut candidate, now));
        assert_eq!(candidate.profile.total_backedges, 801);
        assert_eq!(candidate.profile.hot_series_ns, 0);
        assert_eq!(candidate.profile.active_windows, 0);
        assert_eq!(candidate.windows_this_run, 0);
    }

    #[test]
    fn native_exit_decoder_rejects_unknown_status_and_wrong_boundary() {
        let artifact = CompiledProgram {
            instructions: vec![Instruction::Return],
            ..CompiledProgram::default()
        };
        let plan = FunctionPlan {
            entry: 0,
            end: 1,
            instruction_count: 1,
            block_count: 1,
            edge_count: 0,
            phi_estimate: 0,
            local_count: 0,
            max_stack: 1,
            stack_depths: vec![Some(1)],
            osr_entries: Vec::new(),
        };
        let frame = JitFrame {
            locals: std::ptr::null_mut(),
            stack: std::ptr::null_mut(),
            pc: 0,
            stack_len: 1,
            backedge_budget: 0,
            backedge_count: 0,
            exit_reason: REASON_NORMAL,
        };

        assert_eq!(
            decode_exit(&plan, &artifact, &frame, EXIT_COMPLETED),
            Ok(JitExit::Completed)
        );
        assert!(decode_exit(&plan, &artifact, &frame, 99).is_err());

        let mut wrong_reason = frame;
        wrong_reason.exit_reason = REASON_PROFILE_SLICE;
        assert!(decode_exit(&plan, &artifact, &wrong_reason, EXIT_COMPLETED).is_err());
    }

    #[test]
    fn invalid_persisted_cost_data_is_rejected() {
        let mut coefficients = Coefficients::seeded();
        assert!(coefficients.is_valid());
        coefficients.instructions = f64::NAN;
        assert!(!coefficients.is_valid());

        assert!(
            FunctionProfile {
                compile_ns: 10,
                vm_ns_per_backedge: 20,
                jit_ns_per_backedge: 5,
                calibrated: true,
                ..FunctionProfile::default()
            }
            .is_valid()
        );
        assert!(
            !FunctionProfile {
                calibrated: true,
                ..FunctionProfile::default()
            }
            .is_valid()
        );
    }
}
