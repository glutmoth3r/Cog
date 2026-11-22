# Cog optimizer

Cog checks the resolved program before generating bytecode. Generic instances
have concrete types and use the same code generator as ordinary functions.
The generator uses the checked AST to select inline candidates and to retain
contextual conversions. It then emits the common bytecode for the VM, JIT,
and AOT. Cranelift adds native optimization after lowering.

## Inlining

The code generator summarizes small synchronous functions with scalar
parameters. The summary includes the parameter order, body, supported local
bindings, and node count. Eligible bodies may have conditional expressions
with scoped constant bindings. Calls, mutation, propagation, and cleanup
inside a candidate remain out of scope. This also excludes recursive bodies.
Arguments evaluate once in their original order before entering the inlined
body. The caller's lexical handler and cleanup boundary are suspended during
the body. The caller observes the resulting value or Error at its original
call site.

Automatic inlining allows up to eight nodes at ordinary call sites and up
to thirty inside a loop. The candidate limit is 32 nodes and eight
parameters. A caller can spend up to 256 nodes, reserved before recursively
lowering arguments. Loop nesting is a static heuristic. Use
`@inline("never")` to keep a direct call and compare it with the default.

## Bytecode CFG

The CFG pass tracks successful numeric and scalar constants alongside named
function entries. Each join retains only facts common to every incoming
edge, including loop backedges. Native functions, captures, globals, and
collections can carry callable values, but this pass cannot infer their
targets. A known current-program function with matching arity and no
captures can become a direct call. A callable load immediately preceding
that call in the same block is then redundant. Argument order and the
call's result and propagation marker are preserved. Other uses still
materialize the function value. A modified callback at a loop header can
invalidate the target fact, keeping the call indirect.

The pass rewrites after reaching a fixed point, remaps instruction addresses
and source locations, then compacts local slots using fresh CFG liveness.
Limits bound tracked facts, region size, worklist visits, and slot coloring.
When the final verifier rejects an optimization, the emitter uses the
original bytecode if it validates. No serialized format changes are needed.
Native execution plans are tied to the resulting artifact. APE profiles
already use a bytecode hash, so changing the bytecode yields a different
profile identity.

Use `COG_OPT_TRACE=1` during a fresh source build to see applied inline
decisions and per-function direct call counts. Cached artifacts may skip
source compilation.

## Limits and future work

Instruction and frame budgets apply to the emitted bytecode and its actual
frames. Optimized and reference builds can exhaust those budgets at
different points. Runtime metering continues to prevent native execution
from bypassing limits. The new passes preserve aggregate allocation and
aliasing, so they do not use escape proofs to remove runtime allocations.
General escape analysis would need explicit policy for observable
`exclusive`, heap accounting, and fallback.

Callee bodies with nested calls or effects, range-based check elimination,
loop-invariant code motion, guarded closure specialization, optimization
levels, and AOT profile consumption are future work. The existing APE and
JIT adaptive decisions are unchanged.
