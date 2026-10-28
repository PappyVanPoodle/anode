//! Type and ownership analysis produces checked IR; no backend text is emitted.
pub use crate::cgen::{c_string, Generated};
use crate::{ir::*, syntax::*, Error, Result};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
pub const BUILTINS: &[&str] = &[
    "process_run",
    "send",
    "recv",
    "try_recv",
    "close",
    "is_closed",
    "alloc",
    "free",
    "contains_id",
    "swap",
    "contains_key",
    "insert",
    "remove",
    "clone_get",
    "clone_at",
    "pop",
    "replace",
    "capacity",
    "to_text",
    "f32",
    "f64",
    "sqrt",
    "abs",
    "is_finite",
    "is_nan",
    "parse_f64",
    "is_f64",
    "float_text",
    "char_count",
    "is_char_boundary",
    "codepoint_at",
    "next_char_boundary",
    "codepoint_text",
    "text_find",
    "text_starts_with",
    "text_ends_with",
    "text_trim",
    "text_replace",
    "text_lines",
    "text_join",
    "len",
    "at",
    "push",
    "set",
    "clone",
    "join",
    "monotonic_ms",
    "file_exists",
    "list_dir",
    "exit_code",
    "concat",
    "args_count",
    "arg",
    "read_text",
    "write_text",
    "text_eq",
    "contains",
    "slice",
    "byte_at",
    "parse_int",
    "is_int",
    "int_text",
    "texts",
    "text_at",
    "split",
    "try_read_text",
    "try_write_text",
    "int",
    "i8",
    "i16",
    "i32",
    "i64",
    "u8",
    "u16",
    "u32",
    "u64",
    "saturating_add",
    "saturating_sub",
    "wrapping_add",
    "wrapping_sub",
    "wrapping_mul",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bit_not",
    "shl",
    "shr",
    "rotl",
    "rotr",
    "try_set",
    "read_u16_le",
    "read_u32_le",
    "read_u64_le",
    "read_u16_be",
    "read_u32_be",
    "read_u64_be",
    "wire_read_u16_be",
    "wire_read_u32_be",
    "wire_read_u64_be",
    "try_write_u16_le",
    "try_write_u32_le",
    "try_write_u64_le",
    "try_write_u16_be",
    "try_write_u32_be",
    "try_write_u64_be",
    "wire_try_write_u16_be",
    "wire_try_write_u32_be",
    "wire_try_write_u64_be",
    "fnv1a_64",
    "sha256_text",
    "net_request",
    "try_read_bytes",
    "try_write_bytes",
    "env_var",
    "create_dir",
    "remove_file",
    "net_serve_once",
    "keys",
    "http_request",
];
// New collection operations are contextual so existing user helpers keep their
// meaning. An explicit function in the current module takes precedence.
pub(crate) fn reserved_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
        && ![
            "send",
            "recv",
            "try_recv",
            "close",
            "is_closed",
            "alloc",
            "free",
            "contains_id",
            "swap",
            "clone_at",
            "pop",
            "replace",
            "capacity",
            "contains_key",
            "insert",
            "remove",
            "clone_get",
            "keys",
        ]
        .contains(&name)
}
#[derive(Clone)]
struct Binding {
    place: Place,
    mutable: bool,
    moved: bool,
    moved_fields: BTreeSet<Vec<usize>>,
    loop_depth: usize,
    borrow: Borrow,
    order: usize,
}
struct Value {
    place: Place,
    temporary: bool,
}
struct ScopedLoan {
    place: Place,
    kind: Borrow,
    loop_depth: usize,
    finalizers: Vec<Operation>,
}
struct Checker<'a> {
    funcs: &'a BTreeMap<String, &'a Function>,
    current: &'a Function,
    vars: BTreeMap<String, Binding>,
    types: &'a crate::types::TypeTable,
    locals: Vec<Local>,
    ops: Vec<Operation>,
    loop_depth: usize,
    loop_ids: Vec<usize>,
    next_loop_id: usize,
    borrows: BTreeSet<(usize, Vec<usize>)>,
    exclusive: BTreeSet<(usize, Vec<usize>)>,
    scoped_loans: Vec<ScopedLoan>,
    call_loans: Vec<(Place, Borrow)>,
    summaries: &'a crate::views::Summaries,
    contract: bool,
    probing_holes: bool,
    hole_probes: Vec<HoleProbe>,
    expression_owners: BTreeSet<usize>,
    allow_process: bool,
    allow_net: bool,
}
#[derive(Clone, Debug)]
pub struct HoleProbe {
    pub function: String,
    /// Checker encounter order, not a source identity. Postconditions may be
    /// encountered at multiple returns; editor identities use lexical spans.
    pub ordinal: usize,
    pub line: usize,
    pub declared_type: Type,
    pub intent: String,
    pub context: serde_json::Value,
}
#[derive(Debug, Default)]
pub struct HoleProbeReport {
    pub holes: Vec<HoleProbe>,
    pub diagnostics: Vec<Error>,
}
impl Checker<'_> {
    fn key(p: &Place) -> (usize, Vec<usize>) {
        (p.local, p.fields.clone())
    }
    fn overlaps(set: &BTreeSet<(usize, Vec<usize>)>, p: &Place) -> bool {
        set.iter()
            .any(|(local, fields)| *local == p.local && crate::views::overlaps(fields, &p.fields))
    }
    fn borrowed(&self, p: &Place) -> bool {
        Self::overlaps(&self.borrows, p)
    }
    fn exclusively(&self, p: &Place) -> bool {
        Self::overlaps(&self.exclusive, p)
    }
    fn projected(&self, p: &Place, fields: &[usize]) -> Result<Place> {
        let mut p = p.clone();
        for &index in fields {
            let Type::Named(id) = p.ty else {
                return Err(Error::new(self.current.line, "invalid view projection"));
            };
            let DataKind::Record(members) = &self.types.definitions[id].kind else {
                return Err(Error::new(
                    self.current.line,
                    "view projections require records",
                ));
            };
            p.ty = members
                .get(index)
                .ok_or_else(|| Error::new(self.current.line, "invalid view field"))?
                .ty;
            p.fields.push(index);
        }
        Ok(p)
    }
    fn fresh(&mut self, ty: Type) -> Place {
        let id = self.locals.len();
        self.locals.push(Local {
            ty,
            borrow: Borrow::Owned,
        });
        Place::local(id, ty)
    }
    fn temp(&mut self, ty: Type, value: Rvalue) -> Value {
        // An early `try` return also owns earlier, not-yet-bound arguments and
        // aggregate fields in the same source expression. Forget each operand
        // exactly when ownership is transferred into its new destination.
        let consumed: Vec<&Place> = match &value {
            Rvalue::Call { args, .. } => args
                .iter()
                .filter_map(|a| match a {
                    Argument::Value(p) => Some(p),
                    _ => None,
                })
                .collect(),
            Rvalue::Record(fields) => fields.iter().collect(),
            Rvalue::Variant { payload, .. } => payload.iter().collect(),
            Rvalue::Builtin { name, args }
                if matches!(
                    name.as_str(),
                    "join"
                        | "texts_push"
                        | "vec_push"
                        | "vec_replace"
                        | "arena_alloc"
                        | "map_insert"
                        | "channel_send"
                ) =>
            {
                if name == "map_insert" {
                    args.iter().skip(1).collect()
                } else {
                    args.last().into_iter().collect()
                }
            }
            _ => Vec::new(),
        };
        for p in consumed {
            self.expression_owners.remove(&p.local);
        }
        let place = self.fresh(ty);
        if ty.owned() && !ty.scoped_view() {
            self.expression_owners.insert(place.local);
        }
        self.ops.push(Operation::Init(place.clone(), value));
        Value {
            place,
            temporary: true,
        }
    }
    fn expect(&self, actual: Type, expected: Type, line: usize) -> Result<()> {
        if actual == expected {
            Ok(())
        } else {
            Err(Error::new(
                line,
                format!("expected {expected:?}, found {actual:?}"),
            ))
        }
    }
    fn drop_value(&mut self, v: &Value) {
        if v.temporary && v.place.ty.owned() {
            self.expression_owners.remove(&v.place.local);
            self.ops.push(Operation::Drop(v.place.clone()));
        }
    }
    fn cleanup(&mut self, bindings: impl IntoIterator<Item = Binding>) {
        let mut list: Vec<_> = bindings.into_iter().collect();
        list.sort_by_key(|b| std::cmp::Reverse(b.order));
        for b in list {
            if b.place.ty.owned() && b.borrow == Borrow::Owned {
                self.ops.push(Operation::Drop(b.place));
            }
        }
    }
    fn begin_scoped_loan(&mut self, place: Place, kind: Borrow) {
        self.ops.push(Operation::Loan {
            place: place.clone(),
            kind,
            begin: true,
        });
        self.scoped_loans.push(ScopedLoan {
            place,
            kind,
            loop_depth: self.loop_depth,
            finalizers: Vec::new(),
        });
    }
    fn end_scoped_loan(&mut self, continuing: bool) {
        let loan = self.scoped_loans.pop().expect("balanced lexical loans");
        if continuing {
            self.ops.extend(loan.finalizers);
            self.ops.push(Operation::Loan {
                place: loan.place,
                kind: loan.kind,
                begin: false,
            });
        }
    }
    fn cleanup_loans(&mut self, minimum_loop_depth: usize) {
        // Emit path-local ends without changing the checking stack: a sibling
        // branch is still checked inside the same lexical scopes. A jump out of
        // an inner loop preserves every loan opened outside that loop.
        for loan in self
            .scoped_loans
            .iter()
            .rev()
            .filter(|l| l.loop_depth >= minimum_loop_depth)
        {
            self.ops.extend(loan.finalizers.clone());
            self.ops.push(Operation::Loan {
                place: loan.place.clone(),
                kind: loan.kind,
                begin: false,
            });
        }
    }
    fn get(&self, n: &str, line: usize) -> Result<Binding> {
        self.vars
            .get(n)
            .cloned()
            .ok_or_else(|| Error::new(line, format!("unknown variable {n}")))
    }
    fn place(&self, e: &Expr) -> Result<(String, Binding, Place)> {
        match &e.kind {
            ExprKind::Var(n) => {
                let b = self.get(n, e.line)?;
                Ok((n.clone(), b.clone(), b.place))
            }
            ExprKind::Field(base, name) => {
                let (n, b, mut p) = self.place(base)?;
                let (index, ty) = self.types.field(p.ty, name, e.line)?;
                p.fields.push(index);
                p.ty = ty;
                Ok((n, b, p))
            }
            _ => Err(Error::new(
                e.line,
                "field access requires a named owner; bind the result first",
            )),
        }
    }
    fn live(&self, n: &str, b: &Binding, p: &Place, line: usize) -> Result<()> {
        if b.moved
            || b.moved_fields
                .iter()
                .any(|f| f.starts_with(&p.fields) || p.fields.starts_with(f))
        {
            return Err(Error::new(line,format!("{n} was moved or partially moved; restore that field before using the whole value")));
        }
        Ok(())
    }
    fn view(&mut self, e: &Expr) -> Result<Value> {
        if matches!(e.kind, ExprKind::Var(_) | ExprKind::Field(_, _)) {
            let (n, b, p) = self.place(e)?;
            self.live(&n, &b, &p, e.line)?;
            if self.exclusively(&p) {
                return Err(Error::new(
                    e.line,
                    format!("{n} is exclusively borrowed by this call"),
                ));
            }
            self.borrows.insert(Self::key(&p));
            Ok(Value {
                place: p,
                temporary: false,
            })
        } else {
            self.expr(e)
        }
    }
    #[rustfmt::skip]
    fn expr(&mut self, e: &Expr) -> Result<Value> {
        match &e.kind{
   ExprKind::MethodCall(_,name,_)=>Err(Error::new(e.line,format!("unknown method {name}"))),
   ExprKind::Try(value)=>self.try_value(value,e.line),
   ExprKind::Repeat(value,count)=>{
    let value=self.expr(value)?;
    let n=value.place.ty.integer().ok_or_else(||Error::new(e.line,"fixed arrays require integer elements"))?;
    if *count>65536/(n.bits()/8){return Err(Error::new(e.line,"fixed array exceeds 64 KiB"));}
    Ok(self.temp(Type::Fixed(n,*count),Rvalue::Repeat(value.place,*count)))
   },
   ExprKind::Hole(ty,intent)=>{
    let type_name=|ty:Type| if let Type::Named(id)=ty {self.types.definitions[id].name.clone()}else{ty.label()};
    let scope=self.vars.iter().filter(|(name,_)|!name.starts_with('@')).map(|(name,b)|serde_json::json!({"name":name,"type":type_name(b.place.ty),"mutable":b.mutable,"moved":b.moved,"moved_fields":b.moved_fields,"borrow":format!("{:?}",b.borrow),"borrowed_in_expression":self.borrowed(&b.place),"exclusive_in_expression":self.exclusively(&b.place)})).collect::<Vec<_>>();
    let mut err=Error::new(e.line,"unresolved typed hole; implement it before checking or building");
    let context=serde_json::json!({"kind":"synthesis_hole","declared_type":type_name(*ty),"intent":intent,"scope":scope,"effect_contracts":self.current.effects,"declared_type_is_not_inferred":true,"conditional_on_other_holes":self.probing_holes,"parameter_views":self.current.params.iter().map(|p|serde_json::json!({"name":p.name,"borrow":format!("{:?}",p.borrow),"view":p.view.as_ref().map(|fields|fields.iter().map(|f|serde_json::json!({"path":f.path,"mutable":f.mutable})).collect::<Vec<_>>())})).collect::<Vec<_>>()});
    if self.probing_holes {
        self.hole_probes.push(HoleProbe {function:self.current.name.clone(),ordinal:self.hole_probes.len(),line:e.line,declared_type:*ty,intent:intent.clone(),context});
        return Ok(self.temp(*ty,Rvalue::Zero));
    }
    err.details=Some(context);Err(err)
   },
   ExprKind::Borrow(_,_)=>Err(Error::new(e.line,"references are non-escaping call arguments; they cannot be stored, returned or used as values")),
   ExprKind::Int(n)=>Ok(self.temp(Type::Int,Rvalue::Int(*n))),
   ExprKind::Float(n)=>Ok(self.temp(Type::Float(crate::numeric::Float::F64),Rvalue::Float(*n))),
   ExprKind::Bool(b)=>Ok(self.temp(Type::Bool,Rvalue::Bool(*b))),
   ExprKind::Text(s)=>{if self.contract{return Err(Error::new(e.line,"contracts cannot allocate text"));}Ok(self.temp(Type::Text,Rvalue::Text(s.clone())))},
   ExprKind::Var(_)|ExprKind::Field(_,_)=>{
    let(n,b,p)=self.place(e)?;self.live(&n,&b,&p,e.line)?;
    if self.exclusively(&p){return Err(Error::new(e.line,format!("{n} is exclusively borrowed by this call")));}
    if !p.ty.owned(){return Ok(self.temp(p.ty,Rvalue::Copy(p)));}
    if b.borrow!=Borrow::Owned{return Err(Error::new(e.line,format!("cannot move out of borrowed parameter {n}; use clone for a new owner")));}
    if self.contract{return Err(Error::new(e.line,"contracts cannot move values; use len or at"));}
    if self.borrowed(&p){return Err(Error::new(e.line,format!("cannot move {n} while borrowed by this expression")));}
    if self.loop_depth>b.loop_depth{return Err(Error::new(e.line,format!("cannot move outer-owned {n} inside a repeatable loop; clone it explicitly")));}
    let owner=self.vars.get_mut(&n).unwrap();if p.fields.is_empty(){owner.moved=true;}else{owner.moved_fields.insert(p.fields.clone());}
    let place=self.fresh(p.ty);self.expression_owners.insert(place.local);self.ops.push(Operation::Move{destination:place.clone(),source:p,initialize:true});Ok(Value{place,temporary:true})
   },
   ExprKind::Record(name,fields)=>{
    if self.contract{return Err(Error::new(e.line,"contracts cannot construct owned records"));}
    let id=self.types.id(name).ok_or_else(||Error::new(e.line,"unknown record type"))?;
    let DataKind::Record(expected)=&self.types.definitions[id].kind else{return Err(Error::new(e.line,"enum requires a variant constructor"));};
    let expected=expected.clone();let mut values=vec![None;expected.len()];
    for(name,expr)in fields{let index=expected.iter().position(|f|&f.name==name).ok_or_else(||Error::new(expr.line,format!("unknown field {name}")))?;
     if values[index].is_some(){return Err(Error::new(expr.line,"duplicate field initializer"));}
     let v=self.expr(expr)?;self.expect(v.place.ty,expected[index].ty,expr.line)?;values[index]=Some(v.place);
    }
    if values.iter().any(Option::is_none){return Err(Error::new(e.line,"record initializer must supply every field"));}
    Ok(self.temp(Type::Named(id),Rvalue::Record(values.into_iter().map(Option::unwrap).collect())))
   },
   ExprKind::Array(elements)=>{
    if self.contract{return Err(Error::new(e.line,"contracts cannot allocate arrays"));}
    let values=elements.iter().map(|e|self.expr(e)).collect::<Result<Vec<_>>>()?;
    let element=values.first().map_or(Type::Int,|v|v.place.ty);
    if !matches!(element,Type::Int|Type::Text){return Err(Error::new(e.line,"arrays support int or text elements"));}
    for v in &values{self.expect(v.place.ty,element,e.line)?;}
    let out=self.temp(if element==Type::Text{Type::Texts}else{Type::Ints},Rvalue::Zero);
    for v in values{self.temp(Type::Unit,Rvalue::Builtin{name:if element==Type::Text{"texts_push"}else{"push"}.into(),args:vec![out.place.clone(),v.place]});}Ok(out)
   },
   ExprKind::Unary(op,value)=>{let v=self.expr(value)?;let ty=v.place.ty;if op=="!"{self.expect(ty,Type::Bool,e.line)?;}else if !ty.integer().is_some_and(|n|n.signed())&&!matches!(ty,Type::Float(_)){return Err(Error::new(e.line,"negation requires a signed integer or float"));}Ok(self.temp(ty,Rvalue::Unary(op.clone(),v.place)))},
   ExprKind::Binary(op,left,right)=>{
    if op=="&&" {
     if let Some((value,low,high))=integer_interval(left,right) {
      let (_,_,place)=self.place(value)?;
      // Other widths and floats keep the original type/short-circuit path.
      if place.ty==Type::Int {
       let v=self.expr(value)?;
       return if low>high {Ok(self.temp(Type::Bool,Rvalue::Bool(false)))}
        else {Ok(self.temp(Type::Bool,Rvalue::IntRange{value:v.place,low:low as i64,high:high as i64}))};
      }
     }
    }
    let l=self.expr(left)?;
    if ["&&","||"].contains(&op.as_str()){
     self.expect(l.place.ty,Type::Bool,e.line)?;let out=self.temp(Type::Bool,Rvalue::Copy(l.place.clone()));
     let condition=if op=="||"{self.temp(Type::Bool,Rvalue::Unary("!".into(),l.place)).place}else{l.place};
     let before=self.vars.clone();let parent=std::mem::take(&mut self.ops);
     let r=self.expr(right)?;self.expect(r.place.ty,Type::Bool,e.line)?;
     for(n,b)in &before{if b.moved!=self.vars[n].moved||b.moved_fields!=self.vars[n].moved_fields{return Err(Error::new(e.line,"conditional expression cannot move owned values"));}}
     self.ops.push(Operation::Assign(out.place.clone(),r.place));
     let yes=std::mem::replace(&mut self.ops,parent);self.ops.push(Operation::If{condition,yes,no:Vec::new()});return Ok(out)
    }
    let r=self.expr(right)?;
    let ty=if ["==","!="].contains(&op.as_str()){
     if l.place.ty!=r.place.ty||!(l.place.ty.integer().is_some()||matches!(l.place.ty,Type::Float(_)|Type::Bool|Type::Id(_))){return Err(Error::new(e.line,"equality requires matching numeric types, bool or nominal arena handles"));}Type::Bool
    }else{if l.place.ty.integer().is_none()&&!matches!(l.place.ty,Type::Float(_)){return Err(Error::new(e.line,"arithmetic requires numbers"));}self.expect(r.place.ty,l.place.ty,e.line)?;if ["<",">","<=",">="].contains(&op.as_str()){Type::Bool}else{l.place.ty}};
    let op=if op=="*"&&ty==Type::Int&&(integer_literal(left).is_some()||integer_literal(right).is_some()){"*literal".into()}else{op.clone()};
    Ok(self.temp(ty,Rvalue::Binary(op,l.place,r.place)))
   },
   ExprKind::Call(n,args)=>self.call(n,args,e.line,false),
   ExprKind::Spawn(n,args)=>self.call(n,args,e.line,true),
  }
    }
    fn try_value(&mut self, expression: &Expr, line: usize) -> Result<Value> {
        if self.contract {
            return Err(Error::new(
                line,
                "try cannot return from a contract expression",
            ));
        }
        let value = self.expr(expression)?;
        let (success, error) = self
            .types
            .result_variants(value.place.ty)
            .ok_or_else(|| Error::new(line, "try requires an owned two-variant result enum"))?;
        let (_, target_error) = self
            .types
            .result_variants(self.current.ret)
            .ok_or_else(|| {
                Error::new(
                    line,
                    "try requires an enclosing two-variant result return type",
                )
            })?;
        self.expect(error, target_error, line)?;
        let output = self.temp(success, Rvalue::Zero);
        self.expression_owners.remove(&value.place.local);
        let continuing_owners = self.expression_owners.clone();
        let parent = std::mem::take(&mut self.ops);
        if success != Type::Unit {
            let mut payload = value.place.clone();
            payload.fields.push(0);
            payload.ty = success;
            self.ops.push(if success.owned() {
                Operation::Move {
                    destination: output.place.clone(),
                    source: payload,
                    initialize: false,
                }
            } else {
                Operation::Assign(output.place.clone(), payload)
            });
        }
        self.ops.push(Operation::Drop(value.place.clone()));
        let yes = std::mem::take(&mut self.ops);
        let payload = if error == Type::Unit {
            None
        } else {
            let destination = self.fresh(error);
            let mut source = value.place.clone();
            source.fields.push(1);
            source.ty = error;
            self.ops.push(if error.owned() {
                Operation::Move {
                    destination: destination.clone(),
                    source,
                    initialize: true,
                }
            } else {
                Operation::Init(destination.clone(), Rvalue::Copy(source))
            });
            Some(destination)
        };
        self.ops.push(Operation::Drop(value.place.clone()));
        let returned = self.temp(self.current.ret, Rvalue::Variant { index: 1, payload });
        self.expression_owners.remove(&returned.place.local);
        self.expression_owners.remove(&output.place.local);
        // An unfinished outer call never executes on the error path. End only
        // its actually opened IR loans, in their reverse opening order, before
        // checking return contracts or writing scoped projections back.
        for (place, kind) in self.call_loans.iter().rev() {
            self.ops.push(Operation::Loan {
                place: place.clone(),
                kind: *kind,
                begin: false,
            });
        }
        let saved_borrows = std::mem::take(&mut self.borrows);
        let saved_exclusive = std::mem::take(&mut self.exclusive);
        for loan in &self.scoped_loans {
            self.borrows.insert(Self::key(&loan.place));
            if loan.kind == Borrow::Exclusive {
                self.exclusive.insert(Self::key(&loan.place));
            }
        }
        let postconditions = self.postconditions(Some(&returned));
        self.borrows = saved_borrows;
        self.exclusive = saved_exclusive;
        postconditions?;
        self.cleanup_loans(0);
        for local in self.expression_owners.iter().rev() {
            if self.locals[*local].borrow == Borrow::Owned {
                self.ops.push(Operation::Drop(Place::local(
                    *local,
                    self.locals[*local].ty,
                )));
            }
        }
        self.cleanup(self.vars.values().cloned().collect::<Vec<_>>());
        self.ops.push(Operation::Return(Some(returned.place)));
        let no = std::mem::replace(&mut self.ops, parent);
        self.ops.push(Operation::Switch {
            value: value.place,
            arms: vec![yes, no],
        });
        self.expression_owners = continuing_owners;
        Ok(output)
    }
    fn channel_builtin(&mut self, name: &str, args: &[Expr], line: usize) -> Result<Value> {
        if self.contract && !["len", "capacity", "is_closed"].contains(&name) {
            return Err(Error::new(
                line,
                "contracts cannot mutate, clone or wait on synchronized channels",
            ));
        }
        let count = if matches!(name, "send" | "recv" | "try_recv") {
            2
        } else {
            1
        };
        if args.len() != count {
            return Err(Error::new(
                line,
                format!("{name} expects {count} arguments"),
            ));
        }
        let saved = self.borrows.clone();
        let owner = self.view(&args[0])?;
        let element = self
            .types
            .channel_element(owner.place.ty)
            .ok_or_else(|| Error::new(line, "operation requires a typed channel endpoint"))?;
        let mut values = vec![owner.place.clone()];
        if name == "send" {
            let value = self.expr(&args[1])?;
            self.expect(value.place.ty, element, line)?;
            values.push(value.place);
        } else if matches!(name, "recv" | "try_recv") {
            let ExprKind::Borrow(path, true) = &args[1].kind else {
                return Err(Error::new(
                    line,
                    "receive requires an explicit &mut output slot",
                ));
            };
            let (n, binding, place) = self.place(&crate::views::borrow_place(path, line))?;
            self.live(&n, &binding, &place, line)?;
            self.expect(place.ty, element, line)?;
            if !binding.mutable
                || binding.borrow == Borrow::Shared
                || self.borrowed(&place)
                || self.exclusively(&place)
            {
                return Err(Error::new(
                    line,
                    "receive output requires a live mutable unborrowed slot",
                ));
            }
            values.push(place);
        }
        let ty = match name {
            "close" => Type::Unit,
            "len" | "capacity" => Type::Int,
            "clone" => owner.place.ty,
            _ => Type::Bool,
        };
        let output_loan = if matches!(name, "recv" | "try_recv") {
            values.get(1).cloned()
        } else {
            None
        };
        if let Some(place) = &output_loan {
            self.ops.push(Operation::Loan {
                place: place.clone(),
                kind: Borrow::Exclusive,
                begin: true,
            });
        }
        let out = self.temp(
            ty,
            Rvalue::Builtin {
                name: if name == "clone" {
                    "clone".into()
                } else {
                    format!("channel_{name}")
                },
                args: values,
            },
        );
        if let Some(place) = output_loan {
            self.ops.push(Operation::Loan {
                place,
                kind: Borrow::Exclusive,
                begin: false,
            });
        }
        self.drop_value(&owner);
        self.borrows = saved;
        Ok(out)
    }
    fn call(&mut self, n: &str, args: &[Expr], line: usize, spawn: bool) -> Result<Value> {
        if BUILTINS.contains(&n) && !self.funcs.contains_key(n) {
            if spawn {
                return Err(Error::new(
                    line,
                    "spawn requires a named user function returning an owned value",
                ));
            }
            return self.builtin(n, args, line);
        }
        if self.contract {
            return Err(Error::new(
                line,
                "contract expressions must be pure; user function calls are not allowed",
            ));
        }
        if let Some(id) = self.types.id(n) {
            if self.types.channel_element(Type::Named(id)).is_some() {
                if spawn || args.len() != 1 {
                    return Err(Error::new(
                        line,
                        "channel constructor requires one positive capacity",
                    ));
                }
                let capacity = self.expr(&args[0])?;
                self.expect(capacity.place.ty, Type::Int, line)?;
                return Ok(self.temp(
                    Type::Named(id),
                    Rvalue::Builtin {
                        name: "channel_new".into(),
                        args: vec![capacity.place],
                    },
                ));
            }
            if self.types.vector_element(Type::Named(id)).is_some()
                || self.types.map_types(Type::Named(id)).is_some()
                || self.types.arena_element(Type::Named(id)).is_some()
            {
                if spawn || !args.is_empty() {
                    return Err(Error::new(
                        line,
                        "collection constructor takes no arguments and cannot be spawned",
                    ));
                }
                return Ok(self.temp(Type::Named(id), Rvalue::Zero));
            }
        }
        if let Some((id, index, ty)) = self.types.variant(n) {
            if spawn {
                return Err(Error::new(line, "spawn requires a named user function"));
            }
            if args.len() != usize::from(ty != Type::Unit) {
                return Err(Error::new(line, "variant payload count mismatch"));
            }
            let payload = if ty == Type::Unit {
                None
            } else {
                let v = self.expr(&args[0])?;
                self.expect(v.place.ty, ty, line)?;
                Some(v.place)
            };
            return Ok(self.temp(Type::Named(id), Rvalue::Variant { index, payload }));
        }
        let f = *self
            .funcs
            .get(n)
            .ok_or_else(|| Error::new(line, format!("unknown function {n}")))?;
        if n == "main" {
            return Err(Error::new(line, "main cannot be called or spawned"));
        }
        if spawn && f.external_symbol.is_some() {
            return Err(Error::new(line, "extern functions cannot be worker entry points; call through an owned source wrapper"));
        }
        if args.len() != f.params.len() {
            return Err(Error::new(line, "argument count mismatch"));
        }
        if spawn && self.types.task_of(f.ret).is_none() {
            return Err(Error::new(
                line,
                "workers must return an owned value; views and task handles cannot escape",
            ));
        }
        if spawn
            && f.params.iter().any(|p| {
                p.borrow != Borrow::Owned || p.ty.scoped_view() || p.ty.task_result().is_some()
            })
        {
            return Err(Error::new(
                line,
                "workers must own their data; borrowed parameters cannot cross a thread boundary",
            ));
        }
        let saved = self.borrows.clone();
        let saved_exclusive = self.exclusive.clone();
        let mut values = Vec::new();
        let mut loans = Vec::new();
        let previous_call_loans = self.call_loans.len();
        for (parameter_index, (e, param)) in args.iter().zip(&f.params).enumerate() {
            if param.borrow == Borrow::Owned {
                let v = self.expr(e)?;
                self.expect(v.place.ty, param.ty, line)?;
                values.push(Argument::Value(v.place));
            } else {
                let ExprKind::Borrow(name, mutable) = &e.kind else {
                    return Err(Error::new(
                        e.line,
                        "borrowed argument requires explicit &value or &mut value",
                    ));
                };
                let (owner, b, p) = self.place(&crate::views::borrow_place(name, e.line))?;
                self.expect(p.ty, param.ty, e.line)?;
                let exclusive = param.borrow == Borrow::Exclusive;
                if exclusive != *mutable {
                    return Err(Error::new(
                        e.line,
                        "borrow mutability does not match parameter",
                    ));
                }
                if exclusive && (!b.mutable || b.borrow == Borrow::Shared) {
                    return Err(Error::new(
                        e.line,
                        "exclusive borrowing requires a mutable owner or &mut parameter",
                    ));
                }
                let effects = &self.summaries[n][parameter_index];
                if effects.is_empty() {
                    self.live(&owner, &b, &p, e.line)?;
                }
                for access in effects {
                    let projected = self.projected(&p, &access.fields)?;
                    self.live(&owner, &b, &projected, e.line)?;
                    if self.exclusively(&projected) || (access.mutable && self.borrowed(&projected))
                    {
                        return Err(Error::new(e.line,format!("conflicting borrows of {name} at field path {:?}; pass disjoint fields or narrow the view contract",projected.fields)));
                    }
                    self.borrows.insert(Self::key(&projected));
                    if access.mutable {
                        self.exclusive.insert(Self::key(&projected));
                    }
                    let kind = if access.mutable {
                        Borrow::Exclusive
                    } else {
                        Borrow::Shared
                    };
                    self.ops.push(Operation::Loan {
                        place: projected.clone(),
                        kind,
                        begin: true,
                    });
                    self.call_loans.push((projected.clone(), kind));
                    loans.push((projected, kind));
                }
                values.push(Argument::Reference(p));
            }
        }
        let out = self.temp(
            if spawn {
                self.types.task_of(f.ret).unwrap()
            } else {
                f.ret
            },
            Rvalue::Call {
                name: n.into(),
                args: values,
                spawn,
            },
        );
        for (place, kind) in loans.into_iter().rev() {
            self.ops.push(Operation::Loan {
                place,
                kind,
                begin: false,
            });
        }
        self.call_loans.truncate(previous_call_loans);
        self.borrows = saved;
        self.exclusive = saved_exclusive;
        Ok(out)
    }
    fn wire_builtin(&mut self, n: &str, args: &[Expr], line: usize) -> Result<Value> {
        use crate::numeric::Integer;
        let n = n.strip_prefix("wire_").unwrap_or(n);
        let writing = n.starts_with("try_write_");
        if args.len() != if writing { 3 } else { 2 } {
            return Err(Error::new(
                line,
                "packet operation has the wrong argument count",
            ));
        }
        // Aliases are canonicalized above: the IR only ever names
        // `try_write_u{16,32,64}_{le,be}` / `read_u{16,32,64}_{le,be}`.
        let number = match n {
            "read_u16_le" | "read_u16_be" | "try_write_u16_le" | "try_write_u16_be" => Integer::U16,
            "read_u32_le" | "read_u32_be" | "try_write_u32_le" | "try_write_u32_be" => Integer::U32,
            "read_u64_le" | "read_u64_be" | "try_write_u64_le" | "try_write_u64_be" => Integer::U64,
            _ => unreachable!(),
        };
        if writing && self.contract {
            return Err(Error::new(line, "contracts cannot mutate buffers"));
        }
        let saved = self.borrows.clone();
        let owner = if writing {
            let (name, b, p) = self.place(&args[0])?;
            self.live(&name, &b, &p, line)?;
            if !b.mutable || self.borrowed(&p) {
                return Err(Error::new(
                    line,
                    "packet write requires a live unborrowed mutable buffer",
                ));
            }
            self.borrows.insert(Self::key(&p));
            Value {
                place: p,
                temporary: false,
            }
        } else {
            self.view(&args[0])?
        };
        if !matches!(
            owner.place.ty,
            Type::Fixed(Integer::U8, _) | Type::Slice(Integer::U8)
        ) {
            return Err(Error::new(
                line,
                "packet operations require a fixed u8 buffer",
            ));
        }
        let index = self.expr(&args[1])?;
        self.expect(index.place.ty, Type::Int, line)?;
        let mut values = vec![owner.place.clone(), index.place];
        if writing {
            let value = self.expr(&args[2])?;
            self.expect(value.place.ty, number.ty(), line)?;
            values.push(value.place);
        }
        let out = self.temp(
            if writing { Type::Bool } else { number.ty() },
            Rvalue::Builtin {
                name: n.into(),
                args: values,
            },
        );
        self.drop_value(&owner);
        self.borrows = saved;
        Ok(out)
    }
    /// 1.3 file and environment builtins. Every text argument is borrowed;
    /// `try_read_bytes` writes its live, unborrowed, mutable byte buffer
    /// (argument 1) under the same rules as a packet write, and
    /// `try_write_bytes` only reads its buffer. Capabilities are enforced per
    /// package by `builtin_capability`; all of these are I/O, so contracts
    /// reject them.
    fn os_builtin(&mut self, n: &str, args: &[Expr], line: usize) -> Result<Value> {
        use crate::numeric::Integer;
        let arity = if matches!(n, "try_read_bytes" | "try_write_bytes") {
            2
        } else {
            1
        };
        if args.len() != arity {
            return Err(Error::new(line, format!("{n} expects {arity} arguments")));
        }
        if self.contract {
            return Err(Error::new(line, "contracts cannot allocate or perform I/O"));
        }
        let saved = self.borrows.clone();
        let buffer = if n == "try_read_bytes" {
            let (name, b, p) = self.place(&args[1])?;
            self.live(&name, &b, &p, line)?;
            if !b.mutable || self.borrowed(&p) {
                return Err(Error::new(
                    line,
                    "try_read_bytes requires a live unborrowed mutable [u8; N] or &mut [u8] buffer",
                ));
            }
            self.borrows.insert(Self::key(&p));
            Some(Value {
                place: p,
                temporary: false,
            })
        } else if n == "try_write_bytes" {
            Some(self.view(&args[1])?)
        } else {
            None
        };
        if let Some(buffer) = &buffer {
            if !matches!(
                buffer.place.ty,
                Type::Fixed(Integer::U8, _) | Type::Slice(Integer::U8)
            ) {
                return Err(Error::new(
                    line,
                    format!("{n} requires a [u8; N] or &[u8] buffer"),
                ));
            }
        }
        let path = self.view(&args[0])?;
        if !matches!(path.place.ty, Type::Text | Type::TextView) {
            let what = if n == "env_var" { "name" } else { "path" };
            return Err(Error::new(line, format!("{n} {what} requires text or str")));
        }
        let mut values = vec![path.place.clone()];
        values.extend(buffer.iter().map(|b| b.place.clone()));
        let result = self.temp(
            match n {
                "try_read_bytes" => Type::Int,
                "env_var" => Type::Text,
                _ => Type::Bool,
            },
            Rvalue::Builtin {
                name: n.into(),
                args: values,
            },
        );
        self.drop_value(&path);
        if let Some(buffer) = &buffer {
            self.drop_value(buffer);
        }
        self.borrows = saved;
        Ok(result)
    }
    fn arena_builtin(&mut self, n: &str, args: &[Expr], line: usize) -> Result<Value> {
        if args.len() != 2 {
            return Err(Error::new(line, format!("{n} expects 2 arguments")));
        }
        let mutation = matches!(n, "alloc" | "free");
        if mutation && self.contract {
            return Err(Error::new(
                line,
                "contract expressions cannot mutate arenas",
            ));
        }
        let saved = self.borrows.clone();
        let owner = if mutation {
            let (name, binding, place) = self.place(&args[0])?;
            self.live(&name, &binding, &place, line)?;
            if !binding.mutable
                || binding.borrow == Borrow::Shared
                || self.borrowed(&place)
                || self.exclusively(&place)
            {
                return Err(Error::new(
                    line,
                    "arena mutation requires a live mutable unborrowed owner",
                ));
            }
            self.borrows.insert(Self::key(&place));
            Value {
                place,
                temporary: false,
            }
        } else {
            self.view(&args[0])?
        };
        let element = self
            .types
            .arena_element(owner.place.ty)
            .ok_or_else(|| Error::new(line, "operation requires a typed arena"))?;
        let Type::Named(id) = owner.place.ty else {
            unreachable!()
        };
        if n == "clone_get" && element.owned() && self.contract {
            return Err(Error::new(
                line,
                "contract expressions cannot allocate an owning arena value",
            ));
        }
        let operand = self.expr(&args[1])?;
        self.expect(
            operand.place.ty,
            if n == "alloc" { element } else { Type::Id(id) },
            args[1].line,
        )?;
        let out = self.temp(
            match n {
                "alloc" => Type::Id(id),
                "clone_get" => element,
                _ => Type::Bool,
            },
            Rvalue::Builtin {
                name: format!("arena_{n}"),
                args: vec![owner.place.clone(), operand.place],
            },
        );
        self.drop_value(&owner);
        self.borrows = saved;
        Ok(out)
    }
    fn map_builtin(&mut self, n: &str, args: &[Expr], line: usize) -> Result<Value> {
        let count = if n == "insert" { 3 } else { 2 };
        if args.len() != count {
            return Err(Error::new(line, format!("{n} expects {count} arguments")));
        }
        let mutation = matches!(n, "insert" | "remove");
        if mutation && self.contract {
            return Err(Error::new(line, "contract expressions cannot mutate maps"));
        }
        let saved = self.borrows.clone();
        let owner = if mutation {
            let (name, binding, place) = self.place(&args[0])?;
            self.live(&name, &binding, &place, line)?;
            if !binding.mutable
                || binding.borrow == Borrow::Shared
                || self.borrowed(&place)
                || self.exclusively(&place)
            {
                return Err(Error::new(
                    line,
                    "map mutation requires a live mutable unborrowed owner",
                ));
            }
            // Reserve the complete backing table before evaluating either
            // operand; nested reads are safe, but mutation/moves are not.
            self.borrows.insert(Self::key(&place));
            Value {
                place,
                temporary: false,
            }
        } else {
            self.view(&args[0])?
        };
        if n == "clone_get" {
            if let Some(element) = self.types.arena_element(owner.place.ty) {
                if element.owned() && self.contract {
                    return Err(Error::new(
                        line,
                        "contract expressions cannot allocate an owning arena value",
                    ));
                }
                let Type::Named(id) = owner.place.ty else {
                    unreachable!()
                };
                let handle = self.expr(&args[1])?;
                self.expect(handle.place.ty, Type::Id(id), args[1].line)?;
                let out = self.temp(
                    element,
                    Rvalue::Builtin {
                        name: "arena_clone_get".into(),
                        args: vec![owner.place.clone(), handle.place],
                    },
                );
                self.drop_value(&owner);
                self.borrows = saved;
                return Ok(out);
            }
        }
        let (key_ty, value_ty) = self
            .types
            .map_types(owner.place.ty)
            .ok_or_else(|| Error::new(line, "operation requires a typed map"))?;
        if n == "clone_get" && value_ty.owned() && self.contract {
            return Err(Error::new(
                line,
                "contract expressions cannot allocate an owning map value",
            ));
        }
        let key = if n == "insert" {
            self.expr(&args[1])?
        } else {
            self.view(&args[1])?
        };
        self.expect(key.place.ty, key_ty, args[1].line)?;
        let mut values = vec![owner.place.clone(), key.place.clone()];
        if n == "insert" {
            let value = self.expr(&args[2])?;
            self.expect(value.place.ty, value_ty, args[2].line)?;
            values.push(value.place);
        }
        let out = self.temp(
            match n {
                "insert" => Type::Unit,
                "clone_get" => value_ty,
                _ => Type::Bool,
            },
            Rvalue::Builtin {
                name: format!("map_{n}"),
                args: values,
            },
        );
        if n != "insert" {
            self.drop_value(&key);
        }
        self.drop_value(&owner);
        self.borrows = saved;
        Ok(out)
    }
    /// `keys(m)`: a new `texts` or `ints` holding the keys of a `text`- or
    /// `int`-keyed map, in unspecified order. The map is only borrowed.
    fn map_keys(&mut self, args: &[Expr], line: usize) -> Result<Value> {
        if args.len() != 1 {
            return Err(Error::new(line, "keys expects 1 arguments"));
        }
        if self.contract {
            return Err(Error::new(line, "contracts cannot allocate or perform I/O"));
        }
        let saved = self.borrows.clone();
        let owner = self.view(&args[0])?;
        let (key_ty, _) = self
            .types
            .map_types(owner.place.ty)
            .ok_or_else(|| Error::new(line, "keys requires a typed map"))?;
        let result_ty = match key_ty {
            Type::Text => Type::Texts,
            Type::Int => Type::Ints,
            _ => {
                return Err(Error::new(
                    line,
                    "keys requires a map with text or int keys",
                ))
            }
        };
        let out = self.temp(
            result_ty,
            Rvalue::Builtin {
                name: "map_keys".into(),
                args: vec![owner.place.clone()],
            },
        );
        self.drop_value(&owner);
        self.borrows = saved;
        Ok(out)
    }
    // Dispatch hints never accept source: the selected operation evaluates and
    // checks the expression normally, exactly once, including temporary owners.
    fn expression_type(&self, expression: &Expr) -> Option<Type> {
        match &expression.kind {
            ExprKind::Var(_) | ExprKind::Field(..) => {
                self.place(expression).ok().map(|(_, _, p)| p.ty)
            }
            ExprKind::Call(name, args) => {
                if let Some(function) = self.funcs.get(name) {
                    Some(function.ret)
                } else if let Some(id) = self.types.id(name) {
                    Some(Type::Named(id))
                } else if name == "clone" {
                    args.first().and_then(|e| self.expression_type(e))
                } else if name == "join" {
                    args.first()
                        .and_then(|e| self.expression_type(e))
                        .and_then(Type::task_result)
                } else {
                    None
                }
            }
            ExprKind::Spawn(name, _) => self
                .funcs
                .get(name)
                .and_then(|function| self.types.task_of(function.ret)),
            ExprKind::Try(value) => self
                .expression_type(value)
                .and_then(|ty| self.types.result_variants(ty).map(|v| v.0)),
            _ => None,
        }
    }
    fn builtin(&mut self, n: &str, args: &[Expr], line: usize) -> Result<Value> {
        if let Some(op) = integer_intrinsic(n) {
            let count = if op == 6 { 1 } else { 2 };
            if args.len() != count {
                return Err(Error::new(line, format!("{n} expects {count} arguments")));
            }
            let first = self.expr(&args[0])?;
            if first.place.ty.integer().is_none() {
                return Err(Error::new(line, format!("{n} requires an integer operand")));
            }
            let ty = first.place.ty;
            let mut values = vec![first.place];
            if count == 2 {
                let second = self.expr(&args[1])?;
                self.expect(second.place.ty, if op >= 7 { Type::Int } else { ty }, line)?;
                values.push(second.place);
            }
            return Ok(self.temp(
                ty,
                Rvalue::Builtin {
                    name: n.into(),
                    args: values,
                },
            ));
        }
        if ["send", "recv", "try_recv", "close", "is_closed"].contains(&n) {
            return self.channel_builtin(n, args, line);
        }
        if ["len", "capacity", "clone"].contains(&n)
            && args.len() == 1
            && self
                .expression_type(&args[0])
                .is_some_and(|ty| self.types.channel_element(ty).is_some())
        {
            return self.channel_builtin(n, args, line);
        }
        use Type::*;
        if n == "replace" && args.len() == 3 {
            if let ExprKind::Borrow(name, true) = &args[0].kind {
                let mut args = args.to_vec();
                args[0] = crate::views::borrow_place(name, args[0].line);
                return self.builtin(n, &args, line);
            }
        }
        if n == "swap" && args.len() == 3 {
            if self.contract {
                return Err(Error::new(line, "swap cannot mutate contracts"));
            }
            let ExprKind::Borrow(owner, true) = &args[0].kind else {
                return Err(Error::new(
                    line,
                    "indexed swap requires &mut vector and two indices",
                ));
            };
            let left = format!("@swap_left_{}", self.locals.len());
            let right = format!("@swap_right_{}", self.locals.len());
            self.stmt(&Stmt {
                line,
                kind: StmtKind::ElementPair {
                    names: [left.clone(), right.clone()],
                    owner: crate::views::borrow_place(owner, line),
                    indices: [args[1].clone(), args[2].clone()],
                    body: vec![Stmt {
                        line,
                        kind: StmtKind::Expr(Expr {
                            line,
                            kind: ExprKind::Call(
                                "swap".into(),
                                vec![
                                    Expr {
                                        line,
                                        kind: ExprKind::Var(left),
                                    },
                                    Expr {
                                        line,
                                        kind: ExprKind::Var(right),
                                    },
                                ],
                            ),
                        }),
                    }],
                },
            })?;
            return Ok(self.temp(Unit, Rvalue::Zero));
        }
        if n == "process_run" {
            if !self.allow_process {
                return Err(Error::new(
                    line,
                    "process_run requires the process capability (default false)",
                ));
            }
            if self.contract || args.len() != 2 {
                return Err(Error::new(
                    line,
                    "process_run requires program and borrowed argv outside contracts",
                ));
            }
            let saved = self.borrows.clone();
            let program = self.view(&args[0])?;
            if !matches!(program.place.ty, Text | TextView) {
                return Err(Error::new(line, "process_run program requires text or str"));
            }
            let ExprKind::Borrow(name, false) = &args[1].kind else {
                return Err(Error::new(
                    line,
                    "process_run argv requires a shared borrowed texts or text vector",
                ));
            };
            let argv = self.view(&crate::views::borrow_place(name, args[1].line))?;
            if argv.place.ty != Texts
                && !(self.types.vector_element(argv.place.ty) == Some(Text)
                    && self.types.soa_element(argv.place.ty).is_none())
            {
                return Err(Error::new(
                    line,
                    "process_run argv requires texts or a contiguous text vector",
                ));
            }
            let result_type = self.types.process_result().ok_or_else(|| {
                Error::new(
                    line,
                    "process results require the exact ProcessOutput/ProcessRun prelude layout",
                )
            })?;
            let result = self.temp(
                result_type,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![program.place.clone(), argv.place.clone()],
                },
            );
            self.drop_value(&program);
            self.borrows = saved;
            return Ok(result);
        }
        if n == "net_request" {
            if !self.allow_net {
                return Err(Error::new(
                    line,
                    "net_request requires the net capability (default false)",
                ));
            }
            if self.contract || args.len() != 2 {
                return Err(Error::new(
                    line,
                    "net_request requires an endpoint and a payload outside contracts",
                ));
            }
            // Both texts are borrowed for the call and never consumed.
            let saved = self.borrows.clone();
            let endpoint = self.view(&args[0])?;
            let payload = self.view(&args[1])?;
            if !matches!(endpoint.place.ty, Text | TextView)
                || !matches!(payload.place.ty, Text | TextView)
            {
                return Err(Error::new(
                    line,
                    "net_request endpoint and payload require text or str",
                ));
            }
            let result_type = self.types.net_result().ok_or_else(|| {
                Error::new(
                    line,
                    "net results require the exact NetResponse/NetResult prelude layout",
                )
            })?;
            let result = self.temp(
                result_type,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![endpoint.place.clone(), payload.place.clone()],
                },
            );
            self.drop_value(&endpoint);
            self.drop_value(&payload);
            self.borrows = saved;
            return Ok(result);
        }
        if n == "http_request" {
            // Same capability, borrowing and result as net_request; the
            // runtime reaches any http:// or https:// host through WinHTTP.
            if !self.allow_net {
                return Err(Error::new(
                    line,
                    "http_request requires the net capability (default false)",
                ));
            }
            if self.contract || !(2..=3).contains(&args.len()) {
                return Err(Error::new(
                    line,
                    "http_request requires a url, a body and optional headers outside contracts",
                ));
            }
            let saved = self.borrows.clone();
            let url = self.view(&args[0])?;
            let body = self.view(&args[1])?;
            if !matches!(url.place.ty, Text | TextView) || !matches!(body.place.ty, Text | TextView)
            {
                return Err(Error::new(line, "http_request url and body require text or str"));
            }
            // Optional "Name: value" lines, borrowed for the call.
            let headers = match args.get(2) {
                Some(arg) => {
                    let headers = self.view(arg)?;
                    if headers.place.ty != Type::Texts {
                        return Err(Error::new(
                            line,
                            "http_request headers require texts of \"Name: value\" lines",
                        ));
                    }
                    Some(headers)
                }
                None => None,
            };
            let result_type = self.types.net_result().ok_or_else(|| {
                Error::new(
                    line,
                    "net results require the exact NetResponse/NetResult prelude layout",
                )
            })?;
            let mut places = vec![url.place.clone(), body.place.clone()];
            if let Some(headers) = &headers {
                places.push(headers.place.clone());
            }
            let result = self.temp(
                result_type,
                Rvalue::Builtin {
                    name: n.into(),
                    args: places,
                },
            );
            self.drop_value(&url);
            self.drop_value(&body);
            if let Some(headers) = &headers {
                self.drop_value(headers);
            }
            self.borrows = saved;
            return Ok(result);
        }
        if n == "fnv1a_64" || n == "sha256_text" {
            if args.len() != 1 {
                return Err(Error::new(line, format!("{n} expects 1 arguments")));
            }
            // sha256_text returns a new owned text: it follows the standard
            // rule for allocating text builtins (text_trim, int_text, ...),
            // which contracts reject. fnv1a_64 is pure and allowed.
            if n == "sha256_text" && self.contract {
                return Err(Error::new(line, "contracts cannot allocate or perform I/O"));
            }
            let saved = self.borrows.clone();
            let input = self.view(&args[0])?;
            if !matches!(
                input.place.ty,
                Text | TextView
                    | Slice(crate::numeric::Integer::U8)
                    | Fixed(crate::numeric::Integer::U8, _)
            ) {
                return Err(Error::new(
                    line,
                    format!("{n} requires text, str, [u8; N] or &[u8]"),
                ));
            }
            let result = self.temp(
                if n == "fnv1a_64" {
                    Number(crate::numeric::Integer::U64)
                } else {
                    Text
                },
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![input.place.clone()],
                },
            );
            self.drop_value(&input);
            self.borrows = saved;
            return Ok(result);
        }
        if matches!(
            n,
            "try_read_bytes" | "try_write_bytes" | "env_var" | "create_dir" | "remove_file"
        ) {
            return self.os_builtin(n, args, line);
        }
        if n == "net_serve_once" {
            if !self.allow_net {
                return Err(Error::new(
                    line,
                    "net_serve_once requires the net capability (default false)",
                ));
            }
            if self.contract || args.len() != 3 {
                return Err(Error::new(
                    line,
                    "net_serve_once requires an endpoint, a status and a reply body outside contracts",
                ));
            }
            // Endpoint and reply are borrowed for the call and never consumed.
            let saved = self.borrows.clone();
            let endpoint = self.view(&args[0])?;
            let status = self.expr(&args[1])?;
            self.expect(status.place.ty, Int, line)?;
            let reply = self.view(&args[2])?;
            if !matches!(endpoint.place.ty, Text | TextView)
                || !matches!(reply.place.ty, Text | TextView)
            {
                return Err(Error::new(
                    line,
                    "net_serve_once endpoint and reply body require text or str",
                ));
            }
            let result_type = self.types.net_result().ok_or_else(|| {
                Error::new(
                    line,
                    "net results require the exact NetResponse/NetResult prelude layout",
                )
            })?;
            let result = self.temp(
                result_type,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![endpoint.place.clone(), status.place, reply.place.clone()],
                },
            );
            self.drop_value(&endpoint);
            self.drop_value(&reply);
            self.borrows = saved;
            return Ok(result);
        }
        if ["alloc", "free", "contains_id"].contains(&n) {
            return self.arena_builtin(n, args, line);
        }
        if n == "swap" {
            if self.contract || args.len() != 2 {
                return Err(Error::new(
                    line,
                    "swap requires two mutable places outside contracts",
                ));
            }
            let mut places = Vec::new();
            for e in args {
                let (name, binding, place) = self.place(e)?;
                self.live(&name, &binding, &place, e.line)?;
                if !binding.mutable
                    || binding.borrow == Borrow::Shared
                    || self.borrowed(&place)
                    || self.exclusively(&place)
                {
                    return Err(Error::new(
                        e.line,
                        "swap requires live mutable unborrowed places",
                    ));
                }
                places.push(place);
            }
            self.expect(places[0].ty, places[1].ty, line)?;
            if places[0].local == places[1].local
                && crate::views::overlaps(&places[0].fields, &places[1].fields)
            {
                return Err(Error::new(line, "swap operands must be disjoint"));
            }
            self.ops
                .push(Operation::Swap(places[0].clone(), places[1].clone()));
            return Ok(self.temp(Unit, Rvalue::Zero));
        }
        if ["contains_key", "insert", "remove", "clone_get"].contains(&n) {
            return self.map_builtin(n, args, line);
        }
        if n == "keys" {
            return self.map_keys(args, line);
        }
        if let Some(number) = crate::numeric::Float::parse(n) {
            if args.len() != 1 {
                return Err(Error::new(line, "float conversion expects one argument"));
            }
            let v = self.expr(&args[0])?;
            if v.place.ty.integer().is_none() && !matches!(v.place.ty, Float(_)) {
                return Err(Error::new(line, "float conversion requires a number"));
            }
            return Ok(self.temp(Float(number), Rvalue::Convert(v.place)));
        }
        if ["sqrt", "abs", "is_finite", "is_nan", "float_text"].contains(&n) {
            if args.len() != 1 {
                return Err(Error::new(line, "floating operation expects one argument"));
            }
            if n == "float_text" && self.contract {
                return Err(Error::new(line, "contracts cannot allocate text"));
            }
            let v = self.expr(&args[0])?;
            if !matches!(v.place.ty, Float(_)) {
                return Err(Error::new(line, "floating operation requires f32 or f64"));
            }
            let result = match n {
                "is_finite" | "is_nan" => Bool,
                "float_text" => Text,
                _ => v.place.ty,
            };
            return Ok(self.temp(
                result,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![v.place],
                },
            ));
        }
        if let Some(number) = crate::numeric::Integer::parse(n) {
            if args.len() != 1 {
                return Err(Error::new(line, "integer conversion expects one argument"));
            }
            let value = self.expr(&args[0])?;
            if value.place.ty.integer().is_none() && !matches!(value.place.ty, Float(_)) {
                return Err(Error::new(line, "integer conversion requires a number"));
            }
            return Ok(self.temp(number.ty(), Rvalue::Convert(value.place)));
        }
        if ["saturating_add", "saturating_sub"].contains(&n) {
            if args.len() != 2 {
                return Err(Error::new(
                    line,
                    "saturating arithmetic expects two arguments",
                ));
            }
            let a = self.expr(&args[0])?;
            let b = self.expr(&args[1])?;
            if a.place.ty.integer().is_none() {
                return Err(Error::new(line, "saturating arithmetic requires integers"));
            }
            self.expect(b.place.ty, a.place.ty, line)?;
            return Ok(self.temp(
                a.place.ty,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![a.place, b.place],
                },
            ));
        }
        if n.starts_with("read_u")
            || n.starts_with("wire_read_u")
            || n.starts_with("try_write_u")
            || n.starts_with("wire_try_write_u")
        {
            return self.wire_builtin(n, args, line);
        }
        if n == "texts" {
            if !args.is_empty() {
                return Err(Error::new(line, "texts expects no arguments"));
            }
            return Ok(self.temp(Texts, Rvalue::Zero));
        }
        if ["push", "pop", "replace", "clone_at", "at", "capacity"].contains(&n)
            && !args.is_empty()
            && self
                .place(&args[0])
                .ok()
                .is_some_and(|(_, _, p)| self.types.vector_element(p.ty).is_some())
        {
            let saved = self.borrows.clone();
            let a = self.view(&args[0])?;
            if let Some(element) = self.types.vector_element(a.place.ty) {
                if matches!(a.place.ty, VectorView(_)) && ["push", "pop", "capacity"].contains(&n) {
                    return Err(Error::new(
                        line,
                        "vector regions cannot resize or expose capacity",
                    ));
                }
                let count = match n {
                    "push" | "clone_at" | "at" => 2,
                    "replace" => 3,
                    _ => 1,
                };
                if args.len() != count {
                    return Err(Error::new(line, format!("{n} expects {count} arguments")));
                }
                let mutation = ["push", "pop", "replace"].contains(&n);
                if self.contract && (mutation || (n == "clone_at" && element.owned())) {
                    return Err(Error::new(
                        line,
                        "contract expressions cannot mutate or allocate",
                    ));
                }
                if n == "at" && element.owned() {
                    return Err(Error::new(
                        line,
                        "use clone_at for an explicit owning element copy, or pop to transfer it",
                    ));
                }
                if mutation {
                    self.borrows = saved.clone();
                    let (name, b, p) = self.place(&args[0])?;
                    self.live(&name, &b, &p, line)?;
                    if !b.mutable
                        || b.borrow == Borrow::Shared
                        || self.borrowed(&p)
                        || self.exclusively(&p)
                    {
                        return Err(Error::new(
                            line,
                            "vector mutation requires a live mutable unborrowed owner",
                        ));
                    }
                    self.borrows.insert(Self::key(&p));
                }
                let mut values = vec![a.place.clone()];
                for (i, arg) in args[1..].iter().enumerate() {
                    let expected = if n == "push" || (n == "replace" && i == 1) {
                        element
                    } else {
                        Type::Int
                    };
                    let v = self.expr(arg)?;
                    self.expect(v.place.ty, expected, arg.line)?;
                    values.push(v.place);
                }
                let result = self.temp(
                    if n == "capacity" {
                        Type::Int
                    } else if n == "push" || n == "replace" {
                        Type::Unit
                    } else {
                        element
                    },
                    Rvalue::Builtin {
                        name: format!("vec_{n}"),
                        args: values,
                    },
                );
                self.drop_value(&a);
                self.borrows = saved;
                return Ok(result);
            }
            self.drop_value(&a);
            self.borrows = saved;
            if ["pop", "replace", "clone_at", "capacity"].contains(&n) {
                return Err(Error::new(line, "operation requires a typed vector"));
            }
        }
        if ["pop", "replace", "clone_at", "capacity"].contains(&n) {
            return Err(Error::new(line, "operation requires a named typed vector"));
        }
        let standard: Option<(&[Type], Type, bool)> = match n {
            "to_text" => Some((&[Text], Text, false)),
            "args_count" => Some((&[], Int, false)),
            "monotonic_ms" => Some((&[], Int, false)),
            "file_exists" => Some((&[Text], Bool, false)),
            "list_dir" => Some((&[Text], Texts, false)),
            "exit_code" => Some((&[Int], Unit, false)),
            "arg" => Some((&[Int], Text, false)),
            "read_text" => Some((&[Text], Text, false)),
            "write_text" => Some((&[Text, Text], Unit, false)),
            "text_eq" | "contains" => Some((&[Text, Text], Bool, true)),
            "char_count" => Some((&[Text], Int, true)),
            "is_char_boundary" => Some((&[Text, Int], Bool, true)),
            "codepoint_at" | "next_char_boundary" => Some((&[Text, Int], Int, true)),
            "codepoint_text" => Some((&[Int], Text, false)),
            "text_find" => Some((&[Text, Text, Int], Int, true)),
            "text_starts_with" | "text_ends_with" => Some((&[Text, Text], Bool, true)),
            "text_trim" => Some((&[Text], Text, false)),
            "text_replace" => Some((&[Text, Text, Text], Text, false)),
            "text_lines" => Some((&[Text], Texts, false)),
            "text_join" => Some((&[Texts, Text], Text, false)),
            "slice" => Some((&[Text, Int, Int], Text, false)),
            "byte_at" => Some((&[Text, Int], Int, true)),
            "parse_int" => Some((&[Text], Int, true)),
            // CRT parsing has no language-visible I/O, but no no-allocation
            // contract is assumed for its internal locale/conversion machinery.
            "parse_f64" => Some((&[Text], Float(crate::numeric::Float::F64), false)),
            "is_f64" => Some((&[Text], Bool, false)),
            "is_int" => Some((&[Text], Bool, true)),
            "int_text" => Some((&[Int], Text, false)),
            "text_at" => Some((&[Texts, Int], Text, false)),
            "split" => Some((&[Text, Text], Texts, false)),
            "try_read_text" => Some((
                &[Text],
                Named(self.types.id("ReadResult").ok_or_else(|| {
                    Error::new(line, "read results require project type resolution")
                })?),
                false,
            )),
            "try_write_text" => Some((
                &[Text, Text],
                Named(self.types.id("WriteResult").ok_or_else(|| {
                    Error::new(line, "write results require project type resolution")
                })?),
                false,
            )),
            _ => None,
        };
        if let Some((params, ret, pure)) = standard {
            if args.len() != params.len() {
                return Err(Error::new(
                    line,
                    format!("{n} expects {} arguments", params.len()),
                ));
            }
            if self.contract && !pure {
                return Err(Error::new(line, "contracts cannot allocate or perform I/O"));
            }
            let saved = self.borrows.clone();
            let mut values = Vec::new();
            for (arg, ty) in args.iter().zip(params) {
                let v = self.view(arg)?;
                // Only length-bounded runtime operations may consume non-NUL-terminated views.
                let bounded_text = matches!(
                    n,
                    "to_text"
                        | "text_eq"
                        | "contains"
                        | "char_count"
                        | "is_char_boundary"
                        | "codepoint_at"
                        | "next_char_boundary"
                        | "text_find"
                        | "text_starts_with"
                        | "text_ends_with"
                        | "text_trim"
                        | "text_replace"
                        | "text_lines"
                        | "text_join"
                        | "slice"
                        | "byte_at"
                        | "parse_int"
                        | "is_int"
                );
                if !(bounded_text && *ty == Text && v.place.ty == TextView) {
                    self.expect(v.place.ty, *ty, arg.line)?;
                }
                values.push(v);
            }
            let result = self.temp(
                ret,
                Rvalue::Builtin {
                    name: if n == "contains" && values.iter().any(|v| v.place.ty == TextView) {
                        "view_contains"
                    } else {
                        n
                    }
                    .into(),
                    args: values.iter().map(|v| v.place.clone()).collect(),
                },
            );
            for value in &values {
                self.drop_value(value);
            }
            self.borrows = saved;
            return Ok(result);
        }
        let count = match n {
            "at" | "push" | "concat" => 2,
            "set" | "try_set" => 3,
            _ => 1,
        };
        if args.len() != count {
            return Err(Error::new(line, format!("{n} expects {count} arguments")));
        }
        if self.contract && !["len", "at"].contains(&n) {
            return Err(Error::new(
                line,
                "contract expressions cannot mutate, allocate or join",
            ));
        }
        if n == "join" {
            let v = self.expr(&args[0])?;
            let result = v
                .place
                .ty
                .task_result()
                .ok_or_else(|| Error::new(line, "join expects an owned task handle"))?;
            return Ok(self.temp(
                result,
                Rvalue::Builtin {
                    name: n.into(),
                    args: vec![v.place],
                },
            ));
        }
        if ["push", "set", "try_set"].contains(&n) {
            let (name, b, p) = self.place(&args[0])?;
            self.live(&name, &b, &p, line)?;
            if !b.mutable
                || !matches!(p.ty, Ints | Texts | Fixed(..) | Slice(_))
                || (n == "set" && p.ty == Texts)
                || (n == "push" && matches!(p.ty, Fixed(..) | Slice(_)))
                || (n == "try_set" && !matches!(p.ty, Fixed(..) | Slice(_)))
                || self.borrowed(&p)
            {
                return Err(Error::new(
                    line,
                    "mutation requires a live, unborrowed 'let mut' collection (set supports ints)",
                ));
            }
            let saved = self.borrows.clone();
            self.borrows.insert(Self::key(&p));
            let element = match p.ty {
                Texts => Text,
                Fixed(number, _) | Slice(number) => number.ty(),
                _ => Int,
            };
            let fixed = matches!(p.ty, Fixed(..) | Slice(_));
            let mut values = vec![p];
            for (i, a) in args[1..].iter().enumerate() {
                let v = self.expr(a)?;
                self.expect(
                    v.place.ty,
                    if n != "push" && i == 0 { Int } else { element },
                    line,
                )?;
                values.push(v.place);
            }
            self.borrows = saved;
            return Ok(self.temp(
                if n == "try_set" { Bool } else { Unit },
                Rvalue::Builtin {
                    name: if fixed {
                        if n == "try_set" {
                            "fixed_try_set"
                        } else {
                            "fixed_set"
                        }
                    } else if element == Text {
                        "texts_push"
                    } else {
                        n
                    }
                    .into(),
                    args: values,
                },
            ));
        }
        let saved = self.borrows.clone();
        let a = self.view(&args[0])?;
        let mut values = vec![a.place.clone()];
        let ty = match n {
            "len" => {
                if ![Text, TextView, Ints, Texts].contains(&a.place.ty)
                    && !matches!(a.place.ty, Fixed(..) | Slice(_))
                    && self.types.vector_element(a.place.ty).is_none()
                    && self.types.map_types(a.place.ty).is_none()
                    && self.types.arena_element(a.place.ty).is_none()
                {
                    return Err(Error::new(line, "len expects text or a collection"));
                }
                Int
            }
            "at" => {
                if !matches!(a.place.ty, Ints | Fixed(..) | Slice(_)) {
                    return Err(Error::new(line, "at requires an integer collection"));
                }
                let v = self.expr(&args[1])?;
                self.expect(v.place.ty, Int, line)?;
                values.push(v.place);
                if let Fixed(number, _) | Slice(number) = a.place.ty {
                    number.ty()
                } else {
                    Int
                }
            }
            "clone" => {
                if ![Text, Ints, Texts].contains(&a.place.ty)
                    && !matches!(a.place.ty, Named(_) | Fixed(..))
                {
                    return Err(Error::new(line,"clone supports text, ints or data types; task ownership cannot be duplicated"));
                }
                a.place.ty
            }
            "concat" => {
                if a.place.ty != TextView {
                    self.expect(a.place.ty, Text, line)?;
                }
                let b = self.view(&args[1])?;
                if b.place.ty != TextView {
                    self.expect(b.place.ty, Text, line)?;
                }
                values.push(b.place.clone());
                let result = self.temp(
                    Text,
                    Rvalue::Builtin {
                        name: n.into(),
                        args: values,
                    },
                );
                self.drop_value(&b);
                self.drop_value(&a);
                self.borrows = saved;
                return Ok(result);
            }
            _ => unreachable!(),
        };
        let name = if self.types.arena_element(a.place.ty).is_some() && n == "len" {
            "arena_len"
        } else if self.types.map_types(a.place.ty).is_some() && n == "len" {
            "map_len"
        } else if matches!(a.place.ty, Fixed(..) | Slice(_)) && n == "at" {
            "fixed_at"
        } else if matches!(a.place.ty, Fixed(..) | Slice(_)) && n == "len" {
            "fixed_len"
        } else {
            n
        };
        let result = self.temp(
            ty,
            Rvalue::Builtin {
                name: name.into(),
                args: values,
            },
        );
        self.drop_value(&a);
        self.borrows = saved;
        Ok(result)
    }
    fn contract(&mut self, e: &Expr, label: &str) -> Result<()> {
        self.contract = true;
        let v = self.expr(e)?;
        self.contract = false;
        self.expect(v.place.ty, Type::Bool, e.line)?;
        self.ops.push(Operation::Assert {
            value: v.place,
            line: e.line.saturating_sub(self.current.source_offset),
            message: label.into(),
        });
        Ok(())
    }
    fn postconditions(&mut self, value: Option<&Value>) -> Result<()> {
        if self.current.ensures.is_empty() {
            return Ok(());
        }
        let saved = self.vars.clone();
        self.vars
            .retain(|name, _| self.current.params.iter().any(|p| &p.name == name));
        if let Some(v) = value {
            self.vars.insert(
                "result".into(),
                Binding {
                    place: v.place.clone(),
                    mutable: false,
                    moved: false,
                    moved_fields: BTreeSet::new(),
                    loop_depth: self.loop_depth,
                    borrow: Borrow::Shared,
                    order: self.locals.len(),
                },
            );
        }
        for e in &self.current.ensures {
            self.contract(e, "postcondition failed")?;
        }
        self.vars = saved;
        Ok(())
    }
    fn block(&mut self, body: &[Stmt]) -> Result<bool> {
        let before: BTreeSet<_> = self.vars.keys().cloned().collect();
        let mut returns = false;
        for s in body {
            if returns {
                return Err(Error::new(
                    s.line,
                    "unreachable statement after return, break or continue",
                ));
            }
            returns = self.stmt(s)?;
        }
        let locals: Vec<_> = self
            .vars
            .iter()
            .filter(|(n, _)| !before.contains(*n))
            .map(|(n, b)| (n.clone(), b.clone()))
            .collect();
        if !returns {
            self.cleanup(locals.iter().map(|(_, b)| b.clone()));
        }
        for (n, _) in locals {
            self.vars.remove(&n);
        }
        Ok(returns)
    }
    fn nested(&mut self, body: &[Stmt]) -> Result<(Vec<Operation>, bool)> {
        let parent = std::mem::take(&mut self.ops);
        let returns = self.block(body)?;
        let ops = std::mem::replace(&mut self.ops, parent);
        Ok((ops, returns))
    }
    fn conditional<'s>(
        &mut self,
        mut condition: &'s Expr,
        mut yes: &'s [Stmt],
        mut no: &'s [Stmt],
    ) -> Result<bool> {
        let parent = std::mem::take(&mut self.ops);
        let mut pending = Vec::new();
        let (mut tail, mut returns) = loop {
            let value = self.expr(condition)?;
            self.expect(value.place.ty, Type::Bool, condition.line)?;
            let prefix = std::mem::take(&mut self.ops);
            let initial = self.vars.clone();
            let (yes_code, yes_returns) = self.nested(yes)?;
            let yes_state = self.vars.clone();
            self.vars = initial.clone();
            pending.push((
                prefix,
                value.place,
                initial,
                yes_code,
                yes_returns,
                yes_state,
            ));
            if let [Stmt {
                kind: StmtKind::If(next_condition, next_yes, next_no),
                ..
            }] = no
            {
                condition = next_condition;
                yes = next_yes;
                no = next_no;
            } else {
                break self.nested(no)?;
            }
        };
        // The source is still the frozen nested If AST, but an else-if chain
        // does not need one large checker stack frame per alternative. Each
        // condition's operations stay inside its preceding false branch.
        for (mut prefix, condition, initial, yes, yr, ys) in pending.into_iter().rev() {
            let ns = std::mem::replace(&mut self.vars, initial);
            for (name, binding) in &mut self.vars {
                binding.moved = match (yr, returns) {
                    (true, false) => ns[name].moved,
                    (false, true) => ys[name].moved,
                    _ => ys[name].moved || ns[name].moved,
                };
                binding.moved_fields = match (yr, returns) {
                    (true, false) => ns[name].moved_fields.clone(),
                    (false, true) => ys[name].moved_fields.clone(),
                    _ => ys[name]
                        .moved_fields
                        .union(&ns[name].moved_fields)
                        .cloned()
                        .collect(),
                };
            }
            prefix.push(Operation::If {
                condition,
                yes,
                no: tail,
            });
            tail = prefix;
            returns &= yr;
        }
        self.ops = parent;
        self.ops.extend(tail);
        Ok(returns)
    }
    /// Bounded iteration uses the existing checked while/element machinery.
    /// Bounds have already been evaluated exactly once, in source order.
    fn counted_loop(
        &mut self,
        start: Value,
        end: Value,
        line: usize,
        make_body: impl FnOnce(&str, Expr) -> Vec<Stmt>,
    ) -> Result<()> {
        let index_name = format!("@for_index_{}", self.next_loop_id);
        let limit_name = format!("@for_limit_{}", self.next_loop_id);
        let index = self.temp(Type::Int, Rvalue::Copy(start.place)).place;
        let limit = self.temp(Type::Int, Rvalue::Copy(end.place)).place;
        for (name, place, mutable) in [(&index_name, index, true), (&limit_name, limit, false)] {
            self.vars.insert(
                name.clone(),
                Binding {
                    order: place.local + 1,
                    place,
                    mutable,
                    moved: false,
                    moved_fields: BTreeSet::new(),
                    loop_depth: self.loop_depth,
                    borrow: Borrow::Owned,
                },
            );
        }
        let variable = |name: &str| Expr {
            line,
            kind: ExprKind::Var(name.into()),
        };
        let condition = Expr {
            line,
            kind: ExprKind::Binary(
                "<".into(),
                Box::new(variable(&index_name)),
                Box::new(variable(&limit_name)),
            ),
        };
        let body = make_body(&index_name, variable(&index_name));
        self.stmt(&Stmt {
            line,
            kind: StmtKind::While(condition, body),
        })?;
        self.vars.remove(&index_name);
        self.vars.remove(&limit_name);
        Ok(())
    }
    fn advance_index(name: &str, line: usize) -> Stmt {
        Stmt {
            line,
            kind: StmtKind::Assign(
                name.into(),
                Expr {
                    line,
                    kind: ExprKind::Binary(
                        "+".into(),
                        Box::new(Expr {
                            line,
                            kind: ExprKind::Var(name.into()),
                        }),
                        Box::new(Expr {
                            line,
                            kind: ExprKind::Int(1),
                        }),
                    ),
                },
            ),
        }
    }
    fn stmt(&mut self, s: &Stmt) -> Result<bool> {
        self.expression_owners.clear();
        match &s.kind {
            StmtKind::ForRange {
                name,
                start,
                end,
                body,
            } => {
                if self.vars.contains_key(name) {
                    return Err(Error::new(
                        s.line,
                        "for binding cannot shadow an existing binding",
                    ));
                }
                let start_value = self.expr(start)?;
                self.expect(start_value.place.ty, Type::Int, start.line)?;
                let end_value = self.expr(end)?;
                self.expect(end_value.place.ty, Type::Int, end.line)?;
                self.counted_loop(start_value, end_value, s.line, |index, value| {
                    // Advance before the user body so continue cannot skip it.
                    // An exclusive upper bound makes MAX-1 -> MAX safe.
                    let mut statements = vec![
                        Stmt {
                            line: s.line,
                            kind: StmtKind::Let(name.clone(), false, value),
                        },
                        Self::advance_index(index, s.line),
                    ];
                    statements.extend_from_slice(body);
                    statements
                })?;
            }
            StmtKind::ForEach {
                name,
                mutable,
                owner,
                body,
            } => {
                if self.vars.contains_key(name) {
                    return Err(Error::new(
                        s.line,
                        "for binding cannot shadow an existing binding",
                    ));
                }
                let (owner_name, binding, place) = self.place(owner)?;
                self.live(&owner_name, &binding, &place, s.line)?;
                if self.types.vector_element(place.ty).is_none() {
                    return Err(Error::new(
                        s.line,
                        "for element iteration requires a named typed vector",
                    ));
                }
                if self.exclusively(&place) || (*mutable && self.borrowed(&place)) {
                    return Err(Error::new(
                        s.line,
                        "conflicting iteration loan of the backing vector",
                    ));
                }
                if *mutable && (!binding.mutable || binding.borrow == Borrow::Shared) {
                    return Err(Error::new(
                        s.line,
                        "mutable iteration requires a mutable owner or exclusive parameter",
                    ));
                }
                let start = self.temp(Type::Int, Rvalue::Int(0));
                let end = self.expr(&Expr {
                    line: s.line,
                    kind: ExprKind::Call("len".into(), vec![owner.clone()]),
                })?;
                let position = format!("@for_position_{}", self.next_loop_id);
                self.counted_loop(start, end, s.line, |index, value| {
                    vec![
                        Stmt {
                            line: s.line,
                            kind: StmtKind::Let(position.clone(), false, value),
                        },
                        Self::advance_index(index, s.line),
                        Stmt {
                            line: s.line,
                            kind: StmtKind::Element {
                                access: ElementAccess::Index,
                                name: name.clone(),
                                mutable: *mutable,
                                owner: owner.clone(),
                                index: Expr {
                                    line: s.line,
                                    kind: ExprKind::Var(position),
                                },
                                body: body.clone(),
                            },
                        },
                    ]
                })?;
            }
            StmtKind::ElementPair {
                names,
                owner,
                indices,
                body,
            } => {
                if names[0] == names[1] || names.iter().any(|n| self.vars.contains_key(n)) {
                    return Err(Error::new(
                        s.line,
                        "at_pair requires two fresh distinct binding names",
                    ));
                }
                let (owner_name, binding, p) = self.place(owner)?;
                self.live(&owner_name, &binding, &p, s.line)?;
                let arena = self.types.arena_element(p.ty);
                let element = arena
                    .or_else(|| self.types.vector_element(p.ty))
                    .ok_or_else(|| {
                        Error::new(s.line, "pair borrow requires a typed vector or arena")
                    })?;
                let index_ty = if arena.is_some() {
                    let Type::Named(id) = p.ty else {
                        unreachable!()
                    };
                    Type::Id(id)
                } else {
                    Type::Int
                };
                if !binding.mutable
                    || binding.borrow == Borrow::Shared
                    || self.borrowed(&p)
                    || self.exclusively(&p)
                {
                    return Err(Error::new(
                        s.line,
                        "at_pair requires a live mutable unborrowed vector",
                    ));
                }
                let saved = self.borrows.clone();
                let saved_exclusive = self.exclusive.clone();
                self.borrows.insert(Self::key(&p));
                let left = self.expr(&indices[0])?;
                self.expect(left.place.ty, index_ty, indices[0].line)?;
                let right = self.expr(&indices[1])?;
                self.expect(right.place.ty, index_ty, indices[1].line)?;
                self.ops.push(Operation::ElementCheckPair {
                    owner: p.clone(),
                    left: left.place.clone(),
                    right: right.place.clone(),
                });
                self.exclusive.insert(Self::key(&p));
                self.begin_scoped_loan(p.clone(), Borrow::Exclusive);
                for (name, index) in names.iter().zip([left.place, right.place]) {
                    let alias = self.fresh(element);
                    self.locals[alias.local].borrow = Borrow::Exclusive;
                    self.ops.push(Operation::Init(
                        alias.clone(),
                        Rvalue::Element {
                            owner: p.clone(),
                            index: index.clone(),
                        },
                    ));
                    if self.types.soa_element(p.ty).is_some() {
                        self.scoped_loans.last_mut().unwrap().finalizers.push(
                            Operation::ElementWriteback {
                                alias: alias.clone(),
                                owner: p.clone(),
                                index,
                            },
                        );
                    }
                    self.vars.insert(
                        name.clone(),
                        Binding {
                            place: alias,
                            mutable: true,
                            moved: false,
                            moved_fields: BTreeSet::new(),
                            loop_depth: self.loop_depth,
                            borrow: Borrow::Exclusive,
                            order: self.locals.len(),
                        },
                    );
                }
                let returns = self.block(body)?;
                for name in names {
                    self.vars.remove(name);
                }
                self.end_scoped_loan(!returns);
                self.borrows = saved;
                self.exclusive = saved_exclusive;
                return Ok(returns);
            }
            StmtKind::Element {
                access,
                name,
                mutable,
                owner,
                index,
                body,
            } => {
                if self.vars.contains_key(name) {
                    return Err(Error::new(
                        s.line,
                        "shadowing is not supported in this prototype",
                    ));
                }
                let (owner_name, b, p) = self.place(owner)?;
                self.live(&owner_name, &b, &p, s.line)?;
                let (key_type, element) = match access {
                    ElementAccess::Index => (
                        Type::Int,
                        self.types.vector_element(p.ty).ok_or_else(|| {
                            Error::new(s.line, "at element borrow requires a named typed vector")
                        })?,
                    ),
                    ElementAccess::Key => self
                        .types
                        .map_types(p.ty)
                        .or_else(|| {
                            let element = self.types.arena_element(p.ty)?;
                            let Type::Named(id) = p.ty else { return None };
                            Some((Type::Id(id), element))
                        })
                        .ok_or_else(|| {
                            Error::new(
                                s.line,
                                "get element borrow requires a named typed map or arena",
                            )
                        })?,
                };
                if self.exclusively(&p) || (*mutable && self.borrowed(&p)) {
                    return Err(Error::new(
                        s.line,
                        "conflicting element loan of the backing collection",
                    ));
                }
                if *mutable && (!b.mutable || b.borrow == Borrow::Shared) {
                    return Err(Error::new(
                        s.line,
                        "mutable element borrow requires a mutable owner or exclusive parameter",
                    ));
                }
                let saved = self.borrows.clone();
                let saved_exclusive = self.exclusive.clone();
                // Reserve before the index/key expression, which may call
                // functions. Selecting a slot cannot invalidate its container.
                self.borrows.insert(Self::key(&p));
                let position = if *access == ElementAccess::Key {
                    self.view(index)?
                } else {
                    self.expr(index)?
                };
                self.expect(position.place.ty, key_type, index.line)?;
                // The key is only read while looking up the pointer. Its owner
                // can be reused, moved or changed during the value's lifetime.
                self.borrows = saved.clone();
                self.borrows.insert(Self::key(&p));
                let kind = if *mutable {
                    Borrow::Exclusive
                } else {
                    Borrow::Shared
                };
                if *mutable {
                    self.exclusive.insert(Self::key(&p));
                }
                self.begin_scoped_loan(p.clone(), kind);
                let alias = self.fresh(element);
                self.locals[alias.local].borrow = kind;
                self.ops.push(Operation::Init(
                    alias.clone(),
                    Rvalue::Element {
                        owner: p.clone(),
                        index: position.place.clone(),
                    },
                ));
                if *mutable && self.types.soa_element(p.ty).is_some() {
                    self.scoped_loans.last_mut().unwrap().finalizers.push(
                        Operation::ElementWriteback {
                            alias: alias.clone(),
                            owner: p.clone(),
                            index: position.place.clone(),
                        },
                    );
                }
                self.drop_value(&position);
                self.vars.insert(
                    name.clone(),
                    Binding {
                        place: alias,
                        mutable: *mutable,
                        moved: false,
                        moved_fields: BTreeSet::new(),
                        loop_depth: self.loop_depth,
                        borrow: kind,
                        order: self.locals.len(),
                    },
                );
                let returns = self.block(body)?;
                self.vars.remove(name);
                self.end_scoped_loan(!returns);
                self.borrows = saved;
                self.exclusive = saved_exclusive;
                return Ok(returns);
            }
            StmtKind::Region {
                name,
                mutable,
                owner,
                start,
                end,
                body,
            } => {
                if self.vars.contains_key(name) {
                    return Err(Error::new(
                        s.line,
                        "shadowing is not supported in this prototype",
                    ));
                }
                let (owner_name, b, p) = self.place(owner)?;
                self.live(&owner_name, &b, &p, s.line)?;
                let view_type = match p.ty {
                    Type::Fixed(n, _) | Type::Slice(n) => Type::Slice(n),
                    Type::Ints => Type::Slice(crate::numeric::Integer::I64),
                    Type::VectorView(id) => Type::VectorView(id),
                    Type::Named(id) if self.types.soa_element(p.ty).is_none()
                        && self.types.vector_element(p.ty).is_some() => Type::VectorView(id),
                    Type::Text | Type::TextView if !*mutable => Type::TextView,
                    Type::Text | Type::TextView => {
                        return Err(Error::new(s.line, "text views are immutable"))
                    }
                    _ => {
                        return Err(Error::new(
                            s.line,
                            "region requires a contiguous nominal vector, integer buffer, slice, text or str (SoA is columnar)",
                        ))
                    }
                };
                if self.exclusively(&p) || (*mutable && self.borrowed(&p)) {
                    return Err(Error::new(
                        s.line,
                        "conflicting slice loan of the backing storage",
                    ));
                }
                if *mutable && (!b.mutable || b.borrow == Borrow::Shared) {
                    return Err(Error::new(
                        s.line,
                        "mutable region requires a mutable owner or exclusive slice",
                    ));
                }
                let saved = self.borrows.clone();
                let saved_exclusive = self.exclusive.clone();
                // Reserve before evaluating bounds: they may read, but cannot
                // move, replace, grow or mutate the backing storage.
                self.borrows.insert(Self::key(&p));
                let lo = self.expr(start)?;
                self.expect(lo.place.ty, Type::Int, start.line)?;
                let hi = self.expr(end)?;
                self.expect(hi.place.ty, Type::Int, end.line)?;
                if *mutable {
                    self.exclusive.insert(Self::key(&p));
                }
                let kind = if *mutable {
                    Borrow::Exclusive
                } else {
                    Borrow::Shared
                };
                self.begin_scoped_loan(p.clone(), kind);
                let slice = self.temp(
                    view_type,
                    Rvalue::Region {
                        owner: p.clone(),
                        start: lo.place,
                        end: hi.place,
                    },
                );
                self.vars.insert(
                    name.clone(),
                    Binding {
                        place: slice.place,
                        mutable: *mutable,
                        moved: false,
                        moved_fields: BTreeSet::new(),
                        loop_depth: self.loop_depth,
                        borrow: kind,
                        order: self.locals.len(),
                    },
                );
                let returns = self.block(body)?;
                self.vars.remove(name);
                self.end_scoped_loan(!returns);
                self.borrows = saved;
                self.exclusive = saved_exclusive;
                return Ok(returns);
            }
            StmtKind::Let(n, mutable, e) => {
                if self.vars.contains_key(n) {
                    return Err(Error::new(
                        s.line,
                        "shadowing is not supported in this prototype",
                    ));
                }
                let v = self.expr(e)?;
                if v.place.ty == Type::Unit {
                    return Err(Error::new(s.line, "cannot bind a void result"));
                }
                self.expression_owners.remove(&v.place.local);
                self.vars.insert(
                    n.clone(),
                    Binding {
                        place: v.place,
                        mutable: *mutable,
                        moved: false,
                        moved_fields: BTreeSet::new(),
                        loop_depth: self.loop_depth,
                        borrow: Borrow::Owned,
                        order: self.locals.len(),
                    },
                );
            }
            StmtKind::Assign(n, e) => {
                let b = self.get(n, s.line)?;
                let scoped_alias = b.place.local >= self.current.params.len()
                    && self.locals[b.place.local].borrow == Borrow::Exclusive;
                if !b.mutable
                    || self.borrowed(&b.place)
                    || (b.borrow != Borrow::Owned && !scoped_alias)
                {
                    return Err(Error::new(
                        s.line,
                        "assignment requires an unborrowed mutable binding",
                    ));
                }
                let v = self.expr(e)?;
                self.expect(v.place.ty, b.place.ty, s.line)?;
                if scoped_alias && b.place.ty.owned() {
                    self.ops.push(Operation::Drop(b.place.clone()));
                } else {
                    self.cleanup([b.clone()]);
                }
                if b.place.ty.owned() {
                    self.ops.push(Operation::Move {
                        destination: b.place,
                        source: v.place,
                        initialize: false,
                    });
                } else {
                    self.ops.push(Operation::Assign(b.place, v.place));
                }
                let owner = self.vars.get_mut(n).unwrap();
                owner.moved = false;
                owner.moved_fields.clear();
            }
            StmtKind::Expr(e) => {
                let v = self.expr(e)?;
                self.drop_value(&v);
            }
            StmtKind::Print(e) => {
                let saved = self.borrows.clone();
                let v = self.view(e)?;
                let name = match v.place.ty {
                    Type::Int => "print_int",
                    Type::Float(_) => "print_float",
                    Type::Number(n) => {
                        if n.signed() {
                            "print_int"
                        } else {
                            "print_uint"
                        }
                    }
                    Type::Bool => "print_bool",
                    Type::Text => "print_text",
                    Type::TextView => "print_text_view",
                    _ => return Err(Error::new(s.line, "print accepts int, bool or text")),
                };
                self.temp(
                    Type::Unit,
                    Rvalue::Builtin {
                        name: name.into(),
                        args: vec![v.place.clone()],
                    },
                );
                self.drop_value(&v);
                self.borrows = saved;
            }
            StmtKind::Assert(e) => self.contract(e, "assertion failed")?,
            StmtKind::Break | StmtKind::Continue => {
                let Some(&id) = self.loop_ids.last() else {
                    return Err(Error::new(
                        s.line,
                        "break and continue require an enclosing loop",
                    ));
                };
                // A jump leaves every nested scope inside this loop, but never
                // destroys a parameter or an owner belonging to an outer loop.
                // Moved slots were zeroed by the IR; their drop is harmless.
                self.cleanup_loans(self.loop_depth);
                self.cleanup(
                    self.vars
                        .values()
                        .filter(|b| b.loop_depth >= self.loop_depth)
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                self.ops.push(if matches!(s.kind, StmtKind::Break) {
                    Operation::Break(id)
                } else {
                    Operation::Continue(id)
                });
                return Ok(true);
            }
            StmtKind::Return(e) => {
                let v = e.as_ref().map(|e| self.expr(e)).transpose()?;
                self.expect(
                    v.as_ref().map_or(Type::Unit, |v| v.place.ty),
                    self.current.ret,
                    s.line,
                )?;
                self.postconditions(v.as_ref())?;
                self.cleanup_loans(0);
                self.cleanup(self.vars.values().cloned().collect::<Vec<_>>());
                self.ops.push(Operation::Return(v.map(|v| v.place)));
                return Ok(true);
            }
            StmtKind::If(e, yes, no) => {
                return self.conditional(e, yes, no);
            }
            StmtKind::While(e, body) => {
                let initial = self.vars.clone();
                let id = self.next_loop_id;
                self.next_loop_id += 1;
                self.loop_ids.push(id);
                self.loop_depth += 1;
                let parent = std::mem::take(&mut self.ops);
                let v = self.expr(e)?;
                self.expect(v.place.ty, Type::Bool, s.line)?;
                let condition_code = std::mem::replace(&mut self.ops, parent);
                let (body, _) = self.nested(body)?;
                self.loop_depth -= 1;
                self.loop_ids.pop();
                // The loop may execute zero times. Restoring a previously moved
                // owner inside it cannot make that owner available afterwards.
                // Outer-owner transfers remain prohibited on every loop path,
                // including break/continue paths (the existing conservative rule).
                for (name, b) in &mut self.vars {
                    b.moved |= initial[name].moved;
                    b.moved_fields
                        .extend(initial[name].moved_fields.iter().cloned());
                }
                self.ops.push(Operation::Loop {
                    id,
                    condition_code,
                    condition: v.place,
                    body,
                });
            }
            StmtKind::AssignField(target, e) => {
                let (name, b, p) = self.place(target)?;
                if b.moved || !b.mutable || b.borrow == Borrow::Shared || self.borrowed(&p) {
                    return Err(Error::new(
                        s.line,
                        "field assignment requires a live, unborrowed mutable owner",
                    ));
                }
                if b.moved_fields
                    .iter()
                    .any(|f| p.fields.starts_with(f) && f != &p.fields)
                {
                    return Err(Error::new(
                        s.line,
                        "cannot assign inside a moved parent field",
                    ));
                }
                let saved = self.borrows.clone();
                self.borrows.insert(Self::key(&p));
                let v = self.expr(e)?;
                self.borrows = saved;
                self.expect(v.place.ty, p.ty, s.line)?;
                if p.ty.owned() {
                    self.ops.push(Operation::Drop(p.clone()));
                    self.ops.push(Operation::Move {
                        destination: p.clone(),
                        source: v.place,
                        initialize: false,
                    });
                } else {
                    self.ops.push(Operation::Assign(p.clone(), v.place));
                }
                self.vars
                    .get_mut(&name)
                    .unwrap()
                    .moved_fields
                    .retain(|f| !f.starts_with(&p.fields));
            }
            StmtKind::Match(e, arms) => return self.match_value(e, arms, s.line),
        }
        Ok(false)
    }
}
impl Checker<'_> {
    fn match_value(&mut self, e: &Expr, arms: &[MatchArm], line: usize) -> Result<bool> {
        let saved_borrows = self.borrows.clone();
        let saved_exclusive = self.exclusive.clone();
        let (value, mode) = if let ExprKind::Borrow(name, mutable) = &e.kind {
            let (owner, b, p) = self.place(&crate::views::borrow_place(name, line))?;
            self.live(&owner, &b, &p, line)?;
            if self.exclusively(&p) || (*mutable && self.borrowed(&p)) {
                return Err(Error::new(
                    line,
                    "conflicting match loan of the source enum",
                ));
            }
            if *mutable && (!b.mutable || b.borrow == Borrow::Shared) {
                return Err(Error::new(
                    line,
                    "mutable match requires a mutable owner or exclusive parameter",
                ));
            }
            (
                Value {
                    place: p,
                    temporary: false,
                },
                if *mutable {
                    Borrow::Exclusive
                } else {
                    Borrow::Shared
                },
            )
        } else {
            (self.expr(e)?, Borrow::Owned)
        };
        let Type::Named(id) = value.place.ty else {
            return Err(Error::new(line, "match requires an enum"));
        };
        let DataKind::Enum(variants) = &self.types.definitions[id].kind else {
            return Err(Error::new(line, "match requires an enum"));
        };
        let variants = variants.clone();
        if mode != Borrow::Owned {
            self.borrows.insert(Self::key(&value.place));
            if mode == Borrow::Exclusive {
                self.exclusive.insert(Self::key(&value.place));
            }
            self.begin_scoped_loan(value.place.clone(), mode);
        }
        let initial = self.vars.clone();
        let mut generated = vec![None; variants.len()];
        let mut continuing = Vec::new();
        let mut handled = vec![false; variants.len()];
        let mut dispatch = Vec::new();
        for (position, arm) in arms.iter().enumerate() {
            if arm.path == "_" {
                if arm.binding.is_some() {
                    return Err(Error::new(
                        arm.line,
                        "wildcard match arms cannot bind a payload",
                    ));
                }
                if position + 1 != arms.len() {
                    return Err(Error::new(arm.line, "wildcard match arm must be last"));
                }
                let remaining = variants
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| !handled[*index])
                    .collect::<Vec<_>>();
                if remaining.is_empty() {
                    return Err(Error::new(
                        arm.line,
                        "redundant wildcard match arm; all variants are handled",
                    ));
                }
                // Expand to the existing exhaustive switch. Each owning arm
                // destroys its complete unbound payload; borrowed arms retain
                // the same whole-enum loan and path-specific exit cleanup.
                for (index, variant) in remaining {
                    handled[index] = true;
                    dispatch.push((index, variant.ty, arm));
                }
            } else {
                let Some((owner, index, ty)) = self.types.variant(&arm.path) else {
                    return Err(Error::new(arm.line, "unknown variant in match"));
                };
                if owner != id {
                    return Err(Error::new(
                        arm.line,
                        "match variant belongs to a different enum",
                    ));
                }
                if handled[index] {
                    return Err(Error::new(arm.line, "duplicate match arm"));
                }
                if arm.binding.is_some() != (ty != Type::Unit) {
                    return Err(Error::new(
                        arm.line,
                        "match payload binding does not match variant",
                    ));
                }
                handled[index] = true;
                dispatch.push((index, ty, arm));
            }
        }
        for (index, ty, arm) in dispatch {
            self.vars = initial.clone();
            let parent = std::mem::take(&mut self.ops);
            if let Some(name) = &arm.binding {
                if self.vars.contains_key(name) {
                    return Err(Error::new(arm.line, "match binding shadows a live value"));
                }
                let destination = self.fresh(ty);
                let mut source = value.place.clone();
                source.fields.push(index);
                source.ty = ty;
                if mode != Borrow::Owned {
                    self.locals[destination.local].borrow = mode;
                    self.ops.push(Operation::Init(
                        destination.clone(),
                        Rvalue::Reference(source),
                    ));
                } else if ty.owned() {
                    self.ops.push(Operation::Move {
                        destination: destination.clone(),
                        source,
                        initialize: true,
                    });
                } else {
                    self.ops
                        .push(Operation::Init(destination.clone(), Rvalue::Copy(source)));
                }
                self.vars.insert(
                    name.clone(),
                    Binding {
                        place: destination,
                        mutable: mode == Borrow::Exclusive,
                        moved: false,
                        moved_fields: BTreeSet::new(),
                        loop_depth: self.loop_depth,
                        borrow: mode,
                        order: self.locals.len(),
                    },
                );
            }
            if mode == Borrow::Owned {
                self.ops.push(Operation::Drop(value.place.clone()));
            }
            let returns = self.block(&arm.body)?;
            if !returns {
                if let Some(name) = &arm.binding {
                    self.cleanup([self.vars[name].clone()]);
                }
                continuing.push(self.vars.clone());
            }
            generated[index] = Some(std::mem::replace(&mut self.ops, parent));
        }
        if generated.iter().any(Option::is_none) {
            return Err(Error::new(
                line,
                "non-exhaustive match; every enum variant must be handled",
            ));
        }
        self.vars = initial;
        for (name, b) in &mut self.vars {
            if !continuing.is_empty() {
                b.moved = false;
                b.moved_fields.clear();
                for state in &continuing {
                    b.moved |= state[name].moved;
                    b.moved_fields.extend(state[name].moved_fields.clone());
                }
            }
        }
        self.ops.push(Operation::Switch {
            value: value.place.clone(),
            arms: generated.into_iter().map(Option::unwrap).collect(),
        });
        if mode != Borrow::Owned {
            self.end_scoped_loan(!continuing.is_empty());
        }
        self.borrows = saved_borrows;
        self.exclusive = saved_exclusive;
        Ok(continuing.is_empty())
    }
}
/// Recognize only two constant-bound comparisons of the same scalar place.
/// No calls, indexing, arithmetic or conditional side effects are speculated.
fn integer_interval<'a>(left: &'a Expr, right: &'a Expr) -> Option<(&'a Expr, i128, i128)> {
    fn path(e: &Expr) -> Option<String> {
        match &e.kind {
            ExprKind::Var(name) => Some(name.clone()),
            ExprKind::Field(base, field) => Some(format!("{}.{field}", path(base)?)),
            _ => None,
        }
    }
    fn bound(e: &Expr) -> Option<(&Expr, i128, i128)> {
        let ExprKind::Binary(op, a, b) = &e.kind else {
            return None;
        };
        let (value, constant, swapped) = if let Some(n) = integer_literal(b) {
            (a.as_ref(), i128::from(n), false)
        } else {
            (b.as_ref(), i128::from(integer_literal(a)?), true)
        };
        path(value)?;
        let op = match (op.as_str(), swapped) {
            (">=", false) | ("<=", true) => ">=",
            (">", false) | ("<", true) => ">",
            ("<=", false) | (">=", true) => "<=",
            ("<", false) | (">", true) => "<",
            _ => return None,
        };
        let min = i128::from(i64::MIN);
        let max = i128::from(i64::MAX);
        Some(match op {
            ">=" => (value, constant, max),
            ">" => (value, constant + 1, max),
            "<=" => (value, min, constant),
            "<" => (value, min, constant - 1),
            _ => unreachable!(),
        })
    }
    let (a, low_a, high_a) = bound(left)?;
    let (b, low_b, high_b) = bound(right)?;
    if path(a)? != path(b)? {
        return None;
    }
    Some((a, low_a.max(low_b), high_a.min(high_b)))
}
fn integer_literal(e: &Expr) -> Option<i64> {
    match &e.kind {
        ExprKind::Int(n) => Some(*n),
        ExprKind::Unary(op, v) if op == "-" => integer_literal(v)?.checked_neg(),
        _ => None,
    }
}
pub fn check(functions: &[Function]) -> Result<CheckedProgram> {
    check_with_types(functions, crate::types::TypeTable::default())
}
pub fn check_with_types(
    functions: &[Function],
    types: crate::types::TypeTable,
) -> Result<CheckedProgram> {
    check_with_types_and_ffi(functions, types, false)
}
pub fn check_with_types_and_ffi(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
) -> Result<CheckedProgram> {
    check_with_types_and_capabilities(functions, types, allow_ffi, false)
}
pub fn check_with_types_and_capabilities(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
    allow_process: bool,
) -> Result<CheckedProgram> {
    check_entry(functions, types, true, allow_ffi, allow_process)
}
/// Library compilation retains all body, ownership, view and effect checking.
pub fn check_library_with_types(
    functions: &[Function],
    types: crate::types::TypeTable,
) -> Result<CheckedProgram> {
    check_library_with_types_and_ffi(functions, types, false)
}
pub fn check_library_with_types_and_ffi(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
) -> Result<CheckedProgram> {
    check_library_with_types_and_capabilities(functions, types, allow_ffi, false)
}
pub fn check_library_with_types_and_capabilities(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
    allow_process: bool,
) -> Result<CheckedProgram> {
    check_entry(functions, types, false, allow_ffi, allow_process)
}
fn check_entry(
    functions: &[Function],
    types: crate::types::TypeTable,
    executable: bool,
    allow_ffi: bool,
    allow_process: bool,
) -> Result<CheckedProgram> {
    // Public checker APIs never grant networking; only a project manifest
    // with `capabilities.net = true` can (see `check_owned`).
    Ok(check_entry_mode(
        Cow::Borrowed(functions),
        types,
        executable,
        allow_ffi,
        allow_process,
        false,
        false,
    )?
    .0)
}
/// Conditional checking for editor diagnostics only. The assumed-hole IR is
/// private and discarded here; this API can never return an emittable program.
pub fn probe_holes(
    functions: &[Function],
    types: crate::types::TypeTable,
) -> Result<HoleProbeReport> {
    probe_holes_with_ffi(functions, types, false)
}
pub fn probe_holes_with_ffi(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
) -> Result<HoleProbeReport> {
    probe_holes_with_capabilities(functions, types, allow_ffi, false)
}
pub fn probe_holes_with_capabilities(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
    allow_process: bool,
) -> Result<HoleProbeReport> {
    Ok(check_entry_mode(
        Cow::Borrowed(functions),
        types,
        true,
        allow_ffi,
        allow_process,
        false,
        true,
    )?
    .1)
}
pub fn probe_library_holes_with_ffi(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
) -> Result<HoleProbeReport> {
    probe_library_holes_with_capabilities(functions, types, allow_ffi, false)
}
pub fn probe_library_holes_with_capabilities(
    functions: &[Function],
    types: crate::types::TypeTable,
    allow_ffi: bool,
    allow_process: bool,
) -> Result<HoleProbeReport> {
    Ok(check_entry_mode(
        Cow::Borrowed(functions),
        types,
        false,
        allow_ffi,
        allow_process,
        false,
        true,
    )?
    .1)
}
/// Project resolution already owns a remapped AST. Retain its allocation while
/// method elaboration and every ordinary semantic pass run, then move it to IR.
pub(crate) fn check_owned(
    functions: Vec<Function>,
    types: crate::types::TypeTable,
    executable: bool,
    allow_ffi: bool,
    allow_process: bool,
    allow_net: bool,
) -> Result<CheckedProgram> {
    Ok(check_entry_mode(
        Cow::Owned(functions),
        types,
        executable,
        allow_ffi,
        allow_process,
        allow_net,
        false,
    )?
    .0)
}
pub(crate) fn probe_owned(
    functions: Vec<Function>,
    types: crate::types::TypeTable,
    executable: bool,
    allow_ffi: bool,
    allow_process: bool,
    allow_net: bool,
) -> Result<HoleProbeReport> {
    Ok(check_entry_mode(
        Cow::Owned(functions),
        types,
        executable,
        allow_ffi,
        allow_process,
        allow_net,
        true,
    )?
    .1)
}
fn check_entry_mode(
    functions: Cow<'_, [Function]>,
    types: crate::types::TypeTable,
    executable: bool,
    allow_ffi: bool,
    allow_process: bool,
    allow_net: bool,
    probing_holes: bool,
) -> Result<(CheckedProgram, HoleProbeReport)> {
    profile_work(|profile| profile.semantic_checks += 1);
    let lowered = crate::methods::lower(functions, &types)?;
    let functions = lowered.as_ref();
    let mut funcs = BTreeMap::new();
    let mut foreign_signatures = BTreeMap::new();
    for f in functions {
        if let Some(symbol) = &f.external_symbol {
            if symbol.starts_with("zn_")
                || ["main", "wmain"].contains(&symbol.as_str())
                || !symbol.bytes().enumerate().all(|(i, b)| {
                    b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
                })
                || symbol.is_empty()
            {
                return Err(Error::new(f.line,"extern symbol must be a C identifier outside the reserved zn_ runtime and process-entry namespaces"));
            }
            if !allow_ffi {
                return Err(Error::new(
                    f.line,
                    "extern declarations require the ffi capability (default false)",
                ));
            }
            if !f.effects.is_empty()
                || !f.requires.is_empty()
                || !f.ensures.is_empty()
                || f.method.is_some()
                || f.is_test
                || f.name == "main"
            {
                return Err(Error::new(f.line,"extern functions are a trusted native boundary and cannot carry source effect/view contracts"));
            }
            for ty in f.params.iter().map(|p| p.ty).chain(std::iter::once(f.ret)) {
                if !matches!(
                    ty,
                    Type::Int
                        | Type::Number(_)
                        | Type::Float(_)
                        | Type::Bool
                        | Type::Fixed(..)
                        | Type::Unit
                ) {
                    return Err(Error::new(
                        f.line,
                        "extern ABI supports only scalar and fixed integer buffers",
                    ));
                }
            }
            if f.params
                .iter()
                .any(|p| p.ty == Type::Unit || p.view.is_some())
            {
                return Err(Error::new(
                    f.line,
                    "extern parameters cannot be unit or carry view contracts",
                ));
            }
            let signature = (
                f.ret,
                f.params
                    .iter()
                    .map(|p| (p.ty, p.borrow))
                    .collect::<Vec<_>>(),
            );
            if let Some(previous) = foreign_signatures.insert(symbol.clone(), signature.clone()) {
                if previous != signature {
                    return Err(Error::new(f.line,"extern declarations of the same C symbol must have identical types, lengths and borrowing"));
                }
            }
        }
        if reserved_builtin(&f.name) || funcs.insert(f.name.clone(), f).is_some() {
            return Err(Error::new(f.line, "duplicate or reserved function name"));
        }
    }
    if executable {
        let main = funcs
            .get("main")
            .ok_or_else(|| Error::new(1, "missing fn main()"))?;
        if main.ret != Type::Unit || !main.params.is_empty() {
            return Err(Error::new(
                main.line,
                "main must take no arguments and return no value",
            ));
        }
    }
    let mut bodies = Vec::with_capacity(functions.len());
    let mut report = HoleProbeReport::default();
    let summaries = crate::views::analyze(functions, &types)?;
    for f in functions {
        if f.ret.scoped_view()
            || f.params
                .iter()
                .any(|p| p.ty.scoped_view() && p.borrow == Borrow::Owned)
        {
            return Err(Error::new(
                f.line,
                "scoped views require borrowed parameters and cannot be returned",
            ));
        }
        for ty in f.params.iter().map(|p| p.ty).chain(std::iter::once(f.ret)) {
            types.validate_type(ty, f.line)?;
            if let Type::Named(id) = ty {
                if id >= types.definitions.len() {
                    return Err(Error::new(f.line, "unknown data type in signature"));
                }
            }
        }
        let mut g = Checker {
            funcs: &funcs,
            current: f,
            vars: BTreeMap::new(),
            locals: Vec::with_capacity(f.params.len()),
            ops: Vec::new(),
            loop_depth: 0,
            loop_ids: Vec::new(),
            next_loop_id: 0,
            borrows: BTreeSet::new(),
            exclusive: BTreeSet::new(),
            scoped_loans: Vec::new(),
            call_loans: Vec::new(),
            contract: false,
            types: &types,
            summaries: &summaries,
            probing_holes,
            hole_probes: Vec::new(),
            expression_owners: BTreeSet::new(),
            allow_process,
            allow_net,
        };
        for (i, p) in f.params.iter().enumerate() {
            g.locals.push(Local {
                ty: p.ty,
                borrow: p.borrow,
            });
            if g.vars
                .insert(
                    p.name.clone(),
                    Binding {
                        place: Place::local(i, p.ty),
                        mutable: p.mutable || p.borrow == Borrow::Exclusive,
                        moved: false,
                        moved_fields: BTreeSet::new(),
                        loop_depth: 0,
                        borrow: p.borrow,
                        order: i,
                    },
                )
                .is_some()
            {
                return Err(Error::new(f.line, "duplicate parameter"));
            }
        }
        if f.external_symbol.is_some() {
            bodies.push(Some((g.locals, Vec::new())));
            continue;
        }
        let validation = (|| -> Result<()> {
            for e in &f.requires {
                g.contract(e, "precondition failed")?;
            }
            if !g.block(&f.body)? {
                if f.ret != Type::Unit {
                    return Err(Error::new(
                        f.line,
                        format!("{} does not return a value on every path", f.name),
                    ));
                }
                g.postconditions(None)?;
                g.cleanup(g.vars.values().cloned().collect::<Vec<_>>());
                g.ops.push(Operation::Return(None));
            }
            Ok(())
        })();
        report.holes.append(&mut g.hole_probes);
        if let Err(error) = validation {
            if probing_holes {
                report.diagnostics.push(error);
                bodies.push(None);
                continue;
            }
            return Err(error);
        }
        bodies.push(Some((g.locals, g.ops)));
    }
    let checked = match lowered {
        Cow::Owned(functions) => functions
            .into_iter()
            .zip(bodies)
            .filter_map(|(source, body)| {
                body.map(|(locals, body)| CheckedFunction {
                    source,
                    locals,
                    body,
                })
            })
            .collect(),
        // Diagnostic probes omit invalid bodies. A borrowed caller only needs
        // source ownership for the functions actually retained in private IR.
        Cow::Borrowed(functions) => functions
            .iter()
            .zip(bodies)
            .filter_map(|(source, body)| {
                body.map(|(locals, body)| CheckedFunction {
                    source: source.clone(),
                    locals,
                    body,
                })
            })
            .collect(),
    };
    let program = CheckedProgram {
        functions: checked,
        types,
        views: summaries,
        module_identities: BTreeMap::new(),
        allow_process,
    };
    if let Err(error) = crate::effects::validate(&program) {
        if probing_holes {
            report.diagnostics.push(error);
        } else {
            return Err(error);
        }
    }
    Ok((program, report))
}
pub fn generate(functions: &[Function]) -> Result<String> {
    Ok(generate_parts(functions, true)?.flatten())
}
pub fn generate_parts(functions: &[Function], inline: bool) -> Result<Generated> {
    Ok(crate::cgen::generate(&check(functions)?, inline))
}
