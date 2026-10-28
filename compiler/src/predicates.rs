//! Total, register-only boolean helpers. Checked arithmetic, memory, contracts,
//! loops, reference arguments and unknown calls are deliberately not admitted.
//! Flattening these helpers cannot speculate any observable effect or trap.
use crate::{
    ir::*,
    syntax::{Borrow, Type},
};
use std::{collections::BTreeMap, sync::Arc};

pub(crate) type Expr = Arc<Node>;
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Input(usize),
    Int(i64),
    Bool(bool),
    Compare(String, Type, Expr, Expr),
    Range(Expr, i64, i64),
    Not(Expr),
    And(Expr, Expr),
    Or(Expr, Expr),
    Select(Expr, Expr, Expr),
}
fn not(a: Expr) -> Expr {
    match a.as_ref() {
        Node::Bool(b) => Arc::new(Node::Bool(!b)),
        Node::Not(b) => b.clone(),
        _ => Arc::new(Node::Not(a)),
    }
}
fn select(c: Expr, mut yes: Expr, mut no: Expr) -> Expr {
    if let Node::Bool(b) = c.as_ref() {
        return if *b { yes } else { no };
    }
    if yes == no {
        return yes;
    }
    let inverse = not(c.clone());
    if yes == c {
        yes = Arc::new(Node::Bool(true));
    } else if yes == inverse {
        yes = Arc::new(Node::Bool(false));
    }
    if no == c {
        no = Arc::new(Node::Bool(false));
    } else if no == inverse {
        no = Arc::new(Node::Bool(true));
    }
    match (yes.as_ref(), no.as_ref()) {
        (Node::Bool(true), Node::Bool(false)) => c,
        (Node::Bool(false), Node::Bool(true)) => inverse,
        (Node::Bool(true), _) => Arc::new(Node::Or(c, no)),
        (_, Node::Bool(false)) => Arc::new(Node::And(c, yes)),
        (Node::Bool(false), _) => Arc::new(Node::And(inverse, no)),
        (_, Node::Bool(true)) => Arc::new(Node::Or(inverse, yes)),
        _ => Arc::new(Node::Select(c, yes, no)),
    }
}
fn scalar(t: Type) -> bool {
    matches!(t, Type::Int | Type::Number(_) | Type::Bool)
}
fn read(p: &Place, values: &BTreeMap<usize, Expr>) -> Option<Expr> {
    if !p.fields.is_empty() || !scalar(p.ty) {
        return None;
    }
    values.get(&p.local).cloned()
}
struct Builder<'a> {
    functions: &'a BTreeMap<&'a str, &'a CheckedFunction>,
    stack: Vec<&'a str>,
    budget: usize,
}
impl<'a> Builder<'a> {
    fn function(&mut self, f: &'a CheckedFunction, args: Vec<Expr>) -> Option<Expr> {
        if f.source.ret != Type::Bool
            || self.stack.contains(&f.source.name.as_str())
            || f.source
                .params
                .iter()
                .any(|p| p.borrow != Borrow::Owned || !scalar(p.ty))
        {
            return None;
        }
        self.stack.push(&f.source.name);
        let mut values = args.into_iter().enumerate().collect();
        let result = self.block(&f.body, &mut values)?;
        self.stack.pop();
        result
    }
    fn block(
        &mut self,
        ops: &[Operation],
        values: &mut BTreeMap<usize, Expr>,
    ) -> Option<Option<Expr>> {
        for (i, op) in ops.iter().enumerate() {
            self.budget = self.budget.checked_sub(1)?;
            match op {
                Operation::Init(p, v) if p.fields.is_empty() && scalar(p.ty) => {
                    let value = match v {
                        Rvalue::Int(n) => Arc::new(Node::Int(*n)),
                        Rvalue::Bool(b) => Arc::new(Node::Bool(*b)),
                        Rvalue::Copy(a) => read(a, values)?,
                        Rvalue::Unary(op, a) if op == "!" => not(read(a, values)?),
                        Rvalue::IntRange { value, low, high } => {
                            Arc::new(Node::Range(read(value, values)?, *low, *high))
                        }
                        Rvalue::Binary(op, a, b)
                            if ["==", "!=", "<", "<=", ">", ">="].contains(&op.as_str()) =>
                        {
                            Arc::new(Node::Compare(
                                op.clone(),
                                a.ty,
                                read(a, values)?,
                                read(b, values)?,
                            ))
                        }
                        Rvalue::Call {
                            name,
                            args,
                            spawn: false,
                        } => {
                            let args = args
                                .iter()
                                .map(|arg| match arg {
                                    Argument::Value(a) => read(a, values),
                                    _ => None,
                                })
                                .collect::<Option<Vec<_>>>()?;
                            let f = *self.functions.get(name.as_str())?;
                            self.function(f, args)?
                        }
                        _ => return None,
                    };
                    values.insert(p.local, value);
                }
                Operation::Assign(p, a) if p.fields.is_empty() && scalar(p.ty) => {
                    let value = read(a, values)?;
                    values.insert(p.local, value);
                }
                Operation::If { condition, yes, no } => {
                    let c = read(condition, values)?;
                    let mut y = values.clone();
                    let mut n = values.clone();
                    let yr = self.block(yes, &mut y)?;
                    let nr = self.block(no, &mut n)?;
                    if yr.is_some() || nr.is_some() {
                        let yr = match yr {
                            Some(v) => v,
                            None => self.block(&ops[i + 1..], &mut y)??,
                        };
                        let nr = match nr {
                            Some(v) => v,
                            None => self.block(&ops[i + 1..], &mut n)??,
                        };
                        return Some(Some(select(c, yr, nr)));
                    }
                    // Only bindings visible before the branch can escape it.
                    for (local, value) in values.iter_mut() {
                        *value = select(c.clone(), y.get(local)?.clone(), n.get(local)?.clone());
                    }
                }
                Operation::Return(Some(p)) if p.ty == Type::Bool => {
                    return Some(Some(read(p, values)?))
                }
                // A swap changes both definitions; a projection writeback changes
                // its alias and backing storage. Reject the whole summary rather
                // than retaining expressions derived from their previous values.
                Operation::Swap(..) | Operation::ElementWriteback { .. } => return None,
                _ => return None,
            }
        }
        Some(None)
    }
}
pub(crate) fn analyze(program: &CheckedProgram) -> BTreeMap<String, Expr> {
    let functions = program
        .functions
        .iter()
        .map(|f| (f.source.name.as_str(), f))
        .collect();
    program
        .functions
        .iter()
        .filter_map(|f| {
            let mut builder = Builder {
                functions: &functions,
                stack: Vec::new(),
                budget: 256,
            };
            let args = (0..f.source.params.len())
                .map(|i| Arc::new(Node::Input(i)))
                .collect();
            builder
                .function(f, args)
                .map(|expr| (f.source.name.clone(), expr))
        })
        .collect()
}
