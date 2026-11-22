//! Cog's compiler, portable bytecode, stack VM, and embedding interfaces.
//!
//! Source compilation is coordinated by [`project`]: parsing and resolution
//! produce an AST, compile-time evaluation and semantic checking establish the
//! facts used by [`codegen`], and [`backend`] emits validated instructions.
//! [`interpreter`] is the runtime authority; the internal Cranelift JIT is an
//! optional optimization that falls back to it. User-facing behavior and module
//! boundaries are described in `docs/CONTRIBUTING.md` and `docs/RUNTIME.md`.
pub mod analysis;
mod arguments;
pub mod ast;
mod ast_budget;
pub mod backend;
pub mod build;
pub mod build_inputs;
mod builtins;
pub mod bytecode;
mod comptime;
pub mod diagnostic;
pub mod frontend;
mod generics;
pub mod heap;
mod mutability;
mod optimizer;
mod string_literals;

pub mod codegen;
pub mod core_modules;

pub mod config;
pub mod error;
pub mod hlparser;
pub mod instruction;
pub mod interpreter;
pub(crate) mod jit;
pub mod lexer;
pub mod limits;
mod lint;
pub mod lockfile;
pub mod native;
pub mod package;
pub mod parser;
pub mod project;
pub mod resolver;
pub mod runtime;
pub mod sdk;
pub mod template;
pub mod value;
mod verifier;
pub mod wasm;
