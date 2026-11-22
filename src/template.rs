//! Compact templates for array literals in portable bytecode.
//!
//! Repeated nested elements stay encoded once and are instantiated as fresh
//! managed arrays when the instruction runs. The codec and verifier bound the
//! template shape, and the interpreter separately checks the expanded runtime cost.
use crate::value::Value;

#[derive(Debug, Clone)]
pub enum ArrayTemplate {
    Scalar(Value),
    Array(Vec<ArrayItem>),
}

#[derive(Debug, Clone)]
pub struct ArrayItem {
    pub template: ArrayTemplate,
    pub repeat: usize,
}
