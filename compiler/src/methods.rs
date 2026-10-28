//! Inherent methods elaborate to ordinary calls before ownership and effects.
//! Type hints select a nominal target; the existing checker rechecks the full
//! call, including receiver loans, argument order, views and moves.
use crate::{syntax::*, types::TypeTable, Error, Result};
use std::{borrow::Cow, collections::BTreeMap};

#[derive(Clone)]
struct Signature {
    symbol: String,
    module: String,
    public: bool,
    receiver: Borrow,
    ret: Type,
}
struct Resolver<'a> {
    types: &'a TypeTable,
    functions: BTreeMap<String, Type>,
    methods: BTreeMap<(usize, String), Signature>,
}
type Scope = BTreeMap<String, Type>;

fn place_path(e: &Expr) -> Option<String> {
    match &e.kind {
        ExprKind::Var(n) => Some(n.clone()),
        ExprKind::Field(base, n) => Some(format!("{}.{n}", place_path(base)?)),
        _ => None,
    }
}
impl Resolver<'_> {
    fn path_type(&self, path: &str, scope: &Scope) -> Option<Type> {
        let mut parts = path.split('.');
        let mut ty = *scope.get(parts.next()?)?;
        for field in parts {
            ty = self.types.field(ty, field, 0).ok()?.1;
        }
        Some(ty)
    }
    fn expr(&self, e: &mut Expr, scope: &Scope, module: &str) -> Result<Option<Type>> {
        let spawn = matches!(e.kind, ExprKind::Spawn(..));
        Ok(match &mut e.kind {
            ExprKind::Int(_) => Some(Type::Int),
            ExprKind::Float(_) => Some(Type::Float(crate::numeric::Float::F64)),
            ExprKind::Bool(_) => Some(Type::Bool),
            ExprKind::Text(_) => Some(Type::Text),
            ExprKind::Hole(ty, _) => Some(*ty),
            ExprKind::Try(value) => self
                .expr(value, scope, module)?
                .and_then(|ty| self.types.result_variants(ty).map(|v| v.0)),
            ExprKind::Var(name) => scope.get(name).copied(),
            ExprKind::Borrow(path, _) => self.path_type(path, scope),
            ExprKind::Field(base, name) => self
                .expr(base, scope, module)?
                .and_then(|t| self.types.field(t, name, e.line).ok().map(|v| v.1)),
            ExprKind::Record(name, fields) => {
                for (_, value) in fields {
                    self.expr(value, scope, module)?;
                }
                self.types.id(name).map(Type::Named)
            }
            ExprKind::Array(values) => {
                let mut first = Some(Type::Int);
                for (i, v) in values.iter_mut().enumerate() {
                    let ty = self.expr(v, scope, module)?;
                    if i == 0 {
                        first = ty;
                    }
                }
                match first {
                    Some(Type::Int) => Some(Type::Ints),
                    Some(Type::Text) => Some(Type::Texts),
                    _ => None,
                }
            }
            ExprKind::Repeat(value, count) => self
                .expr(value, scope, module)?
                .and_then(|t| t.integer())
                .map(|n| Type::Fixed(n, *count)),
            ExprKind::Unary(op, value) => {
                let ty = self.expr(value, scope, module)?;
                if op == "!" {
                    Some(Type::Bool)
                } else {
                    ty
                }
            }
            ExprKind::Binary(op, a, b) => {
                let ty = self.expr(a, scope, module)?;
                self.expr(b, scope, module)?;
                if ["==", "!=", "<", ">", "<=", ">=", "&&", "||"].contains(&op.as_str()) {
                    Some(Type::Bool)
                } else {
                    ty
                }
            }
            ExprKind::Call(name, args) | ExprKind::Spawn(name, args) => {
                let hints = args
                    .iter_mut()
                    .map(|v| self.expr(v, scope, module))
                    .collect::<Result<Vec<_>>>()?;
                if spawn {
                    self.functions
                        .get(name)
                        .copied()
                        .and_then(|ty| self.types.task_of(ty))
                } else if let Some(ty) = self.functions.get(name) {
                    Some(*ty)
                } else if let Some((id, _, _)) = self.types.variant(name) {
                    Some(Type::Named(id))
                } else if let Some(id) = self.types.id(name).filter(|&id| {
                    self.types.vector_element(Type::Named(id)).is_some()
                        || self.types.map_types(Type::Named(id)).is_some()
                        || self.types.arena_element(Type::Named(id)).is_some()
                        || self.types.channel_element(Type::Named(id)).is_some()
                }) {
                    Some(Type::Named(id))
                } else {
                    crate::types::builtin_return_type(name, &hints, self.types)
                }
            }
            ExprKind::MethodCall(receiver, name, args) => {
                let ty = self.expr(receiver, scope, module)?.ok_or_else(|| {
                    Error::new(
                        e.line,
                        "cannot determine method receiver type; use a typed named binding",
                    )
                })?;
                for arg in args.iter_mut() {
                    self.expr(arg, scope, module)?;
                }
                let Type::Named(id) = ty else {
                    return Err(Error::new(
                        e.line,
                        "methods require a named record, enum, vector or map",
                    ));
                };
                let signature = self.methods.get(&(id, name.clone())).ok_or_else(|| {
                    Error::new(
                        e.line,
                        format!(
                            "unknown method {name} on {}",
                            self.types.definitions[id].name
                        ),
                    )
                })?;
                if !signature.public && signature.module != module {
                    return Err(Error::new(
                        e.line,
                        format!("method {name} is private; declare pub fn to export it"),
                    ));
                }
                let receiver = if signature.receiver == Borrow::Owned {
                    *std::mem::replace(
                        receiver,
                        Box::new(Expr {
                            line: e.line,
                            kind: ExprKind::Int(0),
                        }),
                    )
                } else {
                    let path = place_path(receiver).ok_or_else(|| {
                        Error::new(
                            e.line,
                            "borrowed method receiver must be named; bind the temporary first",
                        )
                    })?;
                    Expr {
                        line: e.line,
                        kind: ExprKind::Borrow(path, signature.receiver == Borrow::Exclusive),
                    }
                };
                let mut arguments = Vec::with_capacity(args.len() + 1);
                arguments.push(receiver);
                arguments.append(args);
                e.kind = ExprKind::Call(signature.symbol.clone(), arguments);
                Some(signature.ret)
            }
        })
    }
    fn body(&self, body: &mut [Stmt], scope: Scope, module: &str) -> Result<()> {
        // Preserve lexical/source order without a Rust frame for every legal
        // nested statement or else-if when any module declares a method.
        let mut pending = vec![(body.iter_mut(), scope)];
        while let Some((statements, scope)) = pending.last_mut() {
            let Some(statement) = statements.next() else {
                pending.pop();
                continue;
            };
            let mut children = Vec::new();
            match &mut statement.kind {
                StmtKind::ForRange {
                    name,
                    start,
                    end,
                    body,
                } => {
                    self.expr(start, scope, module)?;
                    self.expr(end, scope, module)?;
                    let mut inner = scope.clone();
                    inner.insert(name.clone(), Type::Int);
                    children.push((body.iter_mut(), inner));
                }
                StmtKind::ForEach {
                    name, owner, body, ..
                } => {
                    let ty = self.expr(owner, scope, module)?;
                    let mut inner = scope.clone();
                    if let Some(ty) = ty.and_then(|t| self.types.vector_element(t)) {
                        inner.insert(name.clone(), ty);
                    }
                    children.push((body.iter_mut(), inner));
                }
                StmtKind::Let(name, _, e) => {
                    if let Some(ty) = self.expr(e, scope, module)? {
                        scope.insert(name.clone(), ty);
                    }
                }
                StmtKind::Assign(_, e)
                | StmtKind::Expr(e)
                | StmtKind::Print(e)
                | StmtKind::Assert(e) => {
                    self.expr(e, scope, module)?;
                }
                StmtKind::AssignField(a, b) => {
                    self.expr(a, scope, module)?;
                    self.expr(b, scope, module)?;
                }
                StmtKind::Return(e) => {
                    if let Some(e) = e {
                        self.expr(e, scope, module)?;
                    }
                }
                StmtKind::Break | StmtKind::Continue => {}
                StmtKind::If(e, a, b) => {
                    self.expr(e, scope, module)?;
                    children.push((a.iter_mut(), scope.clone()));
                    children.push((b.iter_mut(), scope.clone()));
                }
                StmtKind::While(e, body) => {
                    self.expr(e, scope, module)?;
                    children.push((body.iter_mut(), scope.clone()));
                }
                StmtKind::Element {
                    access,
                    name,
                    owner,
                    index,
                    body,
                    ..
                } => {
                    let ty = self.expr(owner, scope, module)?;
                    self.expr(index, scope, module)?;
                    let mut inner = scope.clone();
                    if let Some(ty) = ty.and_then(|t| match access {
                        ElementAccess::Index => self.types.vector_element(t),
                        ElementAccess::Key => self
                            .types
                            .map_types(t)
                            .map(|(_, value)| value)
                            .or_else(|| self.types.arena_element(t)),
                    }) {
                        inner.insert(name.clone(), ty);
                    }
                    children.push((body.iter_mut(), inner));
                }
                StmtKind::ElementPair {
                    names,
                    owner,
                    indices,
                    body,
                } => {
                    let ty = self.expr(owner, scope, module)?;
                    for index in indices {
                        self.expr(index, scope, module)?;
                    }
                    let mut inner = scope.clone();
                    if let Some(ty) = ty.and_then(|t| {
                        self.types
                            .vector_element(t)
                            .or_else(|| self.types.arena_element(t))
                    }) {
                        for name in names {
                            inner.insert(name.clone(), ty);
                        }
                    }
                    children.push((body.iter_mut(), inner));
                }
                StmtKind::Region {
                    name,
                    owner,
                    start,
                    end,
                    body,
                    ..
                } => {
                    let ty = self.expr(owner, scope, module)?;
                    self.expr(start, scope, module)?;
                    self.expr(end, scope, module)?;
                    let ty = match ty {
                        Some(Type::Text | Type::TextView) => Some(Type::TextView),
                        Some(Type::Ints) => Some(Type::Slice(crate::numeric::Integer::I64)),
                        Some(Type::Fixed(n, _) | Type::Slice(n)) => Some(Type::Slice(n)),
                        Some(Type::VectorView(id)) => Some(Type::VectorView(id)),
                        Some(Type::Named(id))
                            if self.types.soa_element(Type::Named(id)).is_none()
                                && self.types.vector_element(Type::Named(id)).is_some() =>
                        {
                            Some(Type::VectorView(id))
                        }
                        _ => None,
                    };
                    let mut inner = scope.clone();
                    if let Some(ty) = ty {
                        inner.insert(name.clone(), ty);
                    }
                    children.push((body.iter_mut(), inner));
                }
                StmtKind::Match(e, arms) => {
                    self.expr(e, scope, module)?;
                    for arm in arms {
                        let mut inner = scope.clone();
                        if let (Some(name), Some((_, _, ty))) =
                            (&arm.binding, self.types.variant(&arm.path))
                        {
                            inner.insert(name.clone(), ty);
                        }
                        children.push((arm.body.iter_mut(), inner));
                    }
                }
            }
            pending.extend(children.into_iter().rev());
        }
        Ok(())
    }
}
pub(crate) fn lower<'a>(
    mut functions: Cow<'a, [Function]>,
    types: &TypeTable,
) -> Result<Cow<'a, [Function]>> {
    if !functions.iter().any(|f| f.method.is_some()) {
        return Ok(functions);
    }
    let mut resolver = Resolver {
        types,
        functions: functions.iter().map(|f| (f.name.clone(), f.ret)).collect(),
        methods: BTreeMap::new(),
    };
    for f in functions.iter() {
        if let Some(method) = &f.method {
            let Type::Named(id) = method.owner else {
                return Err(Error::new(f.line, "method owner must be a named type"));
            };
            if types.definitions.get(id).is_none()
                || !f
                    .params
                    .first()
                    .is_some_and(|p| p.name == "self" && p.ty == method.owner)
            {
                return Err(Error::new(f.line, "invalid method receiver"));
            }
            let signature = Signature {
                symbol: f.name.clone(),
                module: f.module.clone(),
                public: f.public,
                receiver: f.params[0].borrow,
                ret: f.ret,
            };
            if resolver
                .methods
                .insert((id, method.name.clone()), signature)
                .is_some()
            {
                return Err(Error::new(f.line, "duplicate method"));
            }
        }
    }
    for f in functions.to_mut() {
        let scope: Scope = f.params.iter().map(|p| (p.name.clone(), p.ty)).collect();
        for e in &mut f.requires {
            resolver.expr(e, &scope, &f.module)?;
        }
        let mut post = scope.clone();
        post.insert("result".into(), f.ret);
        for e in &mut f.ensures {
            resolver.expr(e, &post, &f.module)?;
        }
        resolver.body(&mut f.body, scope, &f.module)?;
    }
    Ok(functions)
}
