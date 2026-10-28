//! Windows x64 C ABI for fully checked libraries. The public ABI contains no
//! language-runtime pointers, owners, views or task handles. Aggregate values
//! cross explicit pointers and are converted field by field in the checked TU.
use crate::{
    ir::CheckedProgram,
    project::Project,
    syntax::{Borrow, DataKind, Function, Type},
    Error, Result,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct LibraryOptions {
    pub optimize: bool,
    pub force: bool,
    /// Shared, bounded object-store budget; zero disables intermediate reuse.
    pub cache_bytes: u64,
    /// Explicit trusted native providers, validated and identity-hashed.
    pub ffi_libraries: Vec<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    pub source_name: String,
    pub symbol: String,
    pub rust_name: String,
}
#[derive(Debug)]
pub struct StaticLibrary {
    pub output: PathBuf,
    pub header: PathBuf,
    pub rust: PathBuf,
    pub exports: Vec<Export>,
    pub bytes: u64,
    pub backend_ms: f64,
    pub runtime_rebuilt: bool,
    pub reused: bool,
}

/// Check a library project and emit `.lib`, `.h` and a standalone Rust module.
/// All source functions are checked before even a whole-output cache hit.
pub fn build_static_lib(
    project: &Project,
    output: &Path,
    options: &LibraryOptions,
) -> Result<StaticLibrary> {
    protect_sources(project, output)?;
    if output
        .extension()
        .is_none_or(|s| !s.eq_ignore_ascii_case("lib"))
    {
        return Err(Error::new(
            0,
            "static library output must have a .lib extension",
        ));
    }
    let link_name = output.file_stem().and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
        .ok_or_else(|| Error::new(0, "static library filename must contain only ASCII letters, digits, underscores or hyphens"))?;
    let checked = project.checked_library()?;
    let prepared = prepare_library(
        project,
        &checked,
        &format!("anode_{}", ident(&project.name)),
        link_name,
    )?;
    let (build, reused) = crate::backend::build_static_archive(
        &prepared.c,
        &prepared.runtime_preamble,
        &prepared.header,
        &prepared.rust,
        output,
        &crate::backend::BuildOptions {
            optimize: options.optimize,
            force: options.force,
            cache_bytes: options.cache_bytes,
            sanitize: false,
            ffi_libraries: options.ffi_libraries.clone(),
        },
    )?;
    Ok(StaticLibrary {
        header: build.output.with_extension("h"),
        rust: build.output.with_extension("rs"),
        output: build.output,
        exports: prepared.exports,
        bytes: build.bytes,
        backend_ms: build.backend_ms,
        runtime_rebuilt: build.runtime_rebuilt,
        reused,
    })
}

pub(crate) struct LibrarySource {
    pub c: String,
    pub header: String,
    pub rust: String,
    pub runtime_preamble: String,
    pub exports: Vec<Export>,
}

/// The same explicit POD/scalar ABI shims serve static archives and reload DLLs.
pub(crate) fn prepare_library(
    project: &Project,
    checked: &CheckedProgram,
    prefix: &str,
    link_name: &str,
) -> Result<LibrarySource> {
    let mut public = Vec::new();
    for (file_id, file) in project.files.iter().enumerate() {
        for (function_id, source) in file.parsed.functions.iter().enumerate() {
            if !source.public
                || source.is_test
                || (source.name == "main" && file.module == file.package)
            {
                continue;
            }
            let internal =
                crate::project::function_symbol(file_id, function_id, &file.module, source);
            let f = checked
                .functions
                .iter()
                .find(|f| f.source.name == internal)
                .ok_or_else(|| {
                    Error::new(source.line, "checked library export is missing").in_file(&file.path)
                })?;
            let path = if let Some(method) = &f.source.method {
                let Type::Named(id) = method.owner else {
                    unreachable!()
                };
                format!("{}::{}", checked.types.definitions[id].name, method.name)
            } else if file.module.is_empty() {
                source.name.clone()
            } else {
                format!("{}::{}", file.module, source.name)
            };
            if project
                .library_exports()
                .is_some_and(|names| !names.contains(&path))
            {
                continue;
            }
            public.push((
                &f.source,
                Export {
                    source_name: path.clone(),
                    symbol: format!("{prefix}_{}", ident(&path)),
                    rust_name: format!("call_{}", ident(&path)),
                },
            ));
        }
    }
    if let Some(selected) = project.library_exports() {
        for name in selected {
            if !public.iter().any(|(_, export)| &export.source_name == name) {
                return Err(Error::new(0, format!("build.exports function {name} is not a public non-entry function in this project")));
            }
        }
    }
    if public.is_empty() {
        return Err(Error::new(
            0,
            "static library has no public functions to export",
        ));
    }
    let mut abi = Abi {
        program: checked,
        prefix,
        types: BTreeSet::new(),
    };
    for (function, export) in &public {
        for (position, ty) in function
            .params
            .iter()
            .map(|p| (format!("parameter {}", p.name), p.ty))
            .chain(std::iter::once(("return value".into(), function.ret)))
        {
            abi.validate(
                ty,
                ty == Type::Unit && position == "return value",
                &position,
            )
            .map_err(|e| {
                project.map_error(Error::new(
                    function.line,
                    format!("C ABI export {}: {}", export.source_name, e.message),
                ))
            })?;
        }
    }
    let (mut header, mut rust, mut conversions) = abi.declarations(link_name);
    header.insert_str(0, "/* Identical functions and immutable strings may share addresses.\n   Native hosts/providers use exported symbols, not address uniqueness, for identity. */\n");
    rust.insert_str(0, "// Identical native functions and immutable strings may share addresses.\n// Use exported symbols rather than address uniqueness for identity.\n");
    if checked
        .functions
        .iter()
        .any(|f| f.source.external_symbol.is_some())
    {
        header.insert_str(0,"/* Trusted native providers are linked into this library. They must honor\n   their declared ABI, buffer bounds, lifetimes, mutability and thread rules.\n   Anode checks source callers; it cannot prove native-body memory safety,\n   purity, ownership behavior or absence of retained pointers/callbacks. */\n");
        rust.insert_str(0,"// This library calls trusted native providers. Providers must honor their\n// declared ABI, buffer bounds, lifetimes, mutability and thread rules.\n// Source checking cannot prove their memory safety, purity, ownership behavior\n// or absence of retained pointers/callbacks. The wrapper relies on that contract.\n");
    }
    let mut raw_rust =
        format!("#[link(name = {link_name:?}, kind = \"static\")]\nunsafe extern \"C\" {{\n");
    header.push_str(&format!("/* Hosts embedding this library in an unloadable DLL call this after all\n   calls/callbacks are quiescent and before FreeLibrary, outside the loader\n   lock. Ordinary executable hosts receive automatic process-exit cleanup. */\nvoid __cdecl {prefix}_runtime_zn_task_pool_shutdown(void);\n"));
    raw_rust.push_str(&format!("    /// Stop workers before unloading a host DLL, outside the loader lock.\n    /// Safety: all library calls, state borrows and callbacks must be quiescent.\n    #[link_name = \"{prefix}_runtime_zn_task_pool_shutdown\"]\n    pub fn runtime_shutdown();\n"));
    let mut safe_rust = String::new();
    for (index, (function, export)) in public.iter().enumerate() {
        abi.function(
            function,
            export,
            index,
            &mut header,
            &mut conversions,
            &mut raw_rust,
            &mut safe_rust,
        );
    }
    header.push_str(&format!("#ifdef __cplusplus\n}}\n#endif\n#pragma pack(pop)\n#undef {prefix}_CALL\n#undef {prefix}_ASSERT\n#endif\n"));
    raw_rust.push_str("}\n");
    rust.push_str(&raw_rust);
    rust.push_str(&safe_rust);
    let generated = Project::c_parts(checked, false);
    let runtime_preamble = runtime_namespace(prefix);
    let mut c = runtime_preamble.clone();
    // Module-local source names and helper names must not collide when two
    // independently built Anode libraries are linked into the same process.
    for f in &checked.functions {
        for role in ["fn", "spawn", "worker"] {
            c.push_str(&format!(
                "#define zn_{role}_{} {prefix}_private_{role}_{}\n",
                f.source.name,
                ident(&f.source.name)
            ));
        }
    }
    c.push_str(&generated.preamble);
    for (_, body) in &generated.functions {
        c.push_str(body);
    }
    // Intentionally omit Generated::entry: a library never owns wmain/main.
    c.push_str(&header);
    c.push_str(&conversions);
    // Codegen's ordinary cross-module declarations have external linkage.
    // This TU contains the full library, so only public ABI shims need it.
    for f in &checked.functions {
        for signature in [
            format!("{} zn_fn_{}(", f.source.ret.c(), f.source.name),
            format!("zn_task* zn_spawn_{}(", f.source.name),
            format!("void zn_worker_{}(", f.source.name),
        ] {
            // Only generated declaration/definition line starts are eligible;
            // source string contents and call expressions are never rewritten.
            c = c.replace(&format!("\n{signature}"), &format!("\nstatic {signature}"));
        }
    }
    Ok(LibrarySource {
        c,
        header,
        rust,
        runtime_preamble,
        exports: public.into_iter().map(|(_, e)| e).collect(),
    })
}

fn protect_sources(project: &Project, output: &Path) -> Result<()> {
    let mut protected: Vec<_> = project.files.iter().map(|f| f.path.clone()).collect();
    protected.extend(project.manifest_paths());
    for artifact in [
        output.to_owned(),
        output.with_extension("h"),
        output.with_extension("rs"),
        output.with_extension("zn-lib-build"),
        output.with_extension("zn-lock"),
    ] {
        if let Ok(actual) = artifact.canonicalize() {
            if protected
                .iter()
                .filter_map(|p| p.canonicalize().ok())
                .any(|p| p == actual)
            {
                return Err(Error::new(
                    0,
                    "static library artifacts must not overwrite project sources or manifest",
                )
                .in_file(artifact));
            }
        }
    }
    Ok(())
}

/// Injective identifier encoding; in particular `_`, module separators and
/// punctuation never collapse to the same spelling.
fn ident(source: &str) -> String {
    let mut out = String::new();
    for b in source.bytes() {
        if b.is_ascii_alphanumeric() {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}
fn runtime_namespace(prefix: &str) -> String {
    // Macro-renaming complete runtime identifiers isolates private runtime
    // state and strong symbols even when hosts use /WHOLEARCHIVE on two libs.
    // C preprocessing leaves strings/comments untouched. Include both files
    // so types, internal helpers and the externally declared ABI agree.
    let mut names = BTreeSet::new();
    for source in [include_str!("runtime.h"), include_str!("runtime.c")] {
        for token in source.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            if token.starts_with("zn_") {
                names.insert(token);
            }
        }
    }
    names
        .into_iter()
        .map(|name| format!("#define {name} {prefix}_runtime_{name}\n"))
        .collect()
}
fn aggregate(ty: Type) -> bool {
    matches!(ty, Type::Fixed(..) | Type::Named(_) | Type::Id(_))
}
/// Follow owned fields/element payloads, but never copyable arena IDs. This is
/// shared by export and reload state validation so an opaque container cannot
/// hide a synchronized endpoint at an ABI boundary.
pub(crate) fn owning_channel(program: &CheckedProgram, ty: Type) -> Option<&str> {
    let mut pending = vec![ty];
    let mut seen = BTreeSet::new();
    while let Some(ty) = pending.pop() {
        let ty = ty.task_result().unwrap_or(ty);
        if let Type::Named(id) = ty {
            if !seen.insert(id) {
                continue;
            }
            let definition = &program.types.definitions[id];
            let fields = match &definition.kind {
                DataKind::Channel(_) => return Some(&definition.name),
                DataKind::Record(f)
                | DataKind::Enum(f)
                | DataKind::Vector(f)
                | DataKind::SoaVector(f)
                | DataKind::Arena(f)
                | DataKind::Map(f) => f,
            };
            pending.extend(fields.iter().map(|f| f.ty));
        }
    }
    None
}
struct Abi<'a> {
    program: &'a CheckedProgram,
    prefix: &'a str,
    types: BTreeSet<Type>,
}
impl Abi<'_> {
    fn validate(&mut self, ty: Type, unit: bool, path: &str) -> Result<()> {
        if let Some(channel) = owning_channel(self.program, ty) {
            return Err(Error::new(0, format!("{path} contains owning channel {channel}; channel endpoints cannot cross this C/Rust ABI")));
        }
        if ty.task_result().is_some() {
            return Err(Error::new(0, format!("{path} uses unsupported task handle {}; task ownership cannot cross this C ABI", ty.label())));
        }
        match ty {
            Type::Int | Type::Number(_) | Type::Float(_) | Type::Bool => (),
            Type::Unit if unit => (),
            Type::Fixed(..) => { self.types.insert(ty); },
            Type::Id(id) => {
                if !self.program.types.definitions[id].public || self.program.types.arena_element(Type::Named(id)).is_none() {return Err(Error::new(0,format!("{path} requires a public nominal arena handle")));}
                self.types.insert(ty);
            },
            Type::Named(id) => {
                let definition = &self.program.types.definitions[id];
                if !definition.public { return Err(Error::new(0, format!("{path} contains private record {}", definition.name))); }
                let DataKind::Record(fields) = &definition.kind else {
                    return Err(Error::new(0, format!("{path} uses opaque or heap-owning type {}; only public POD records are supported", definition.name)));
                };
                if fields.is_empty() { return Err(Error::new(0, format!("{path} uses empty record {}; no portable C layout", definition.name))); }
                if self.types.insert(ty) {
                    for field in fields { self.validate(field.ty, false, &format!("{path}.{}", field.name))?; }
                }
            },
            _ => return Err(Error::new(0, format!("{path} uses unsupported type {}; heap owners, task handles, scoped/escaping views and opaque types cannot cross this C ABI", ty.label()))),
        }
        Ok(())
    }
    fn c(&self, ty: Type) -> String {
        match ty {
            Type::Named(id) => format!(
                "{}_record_{}",
                self.prefix,
                ident(&self.program.types.definitions[id].name)
            ),
            Type::Id(id) => format!(
                "{}_handle_{}",
                self.prefix,
                ident(&self.program.types.definitions[id].name)
            ),
            Type::Fixed(n, count) => format!("{}_buffer_{}_{count}", self.prefix, n.name()),
            Type::Bool => "uint8_t".into(),
            _ => ty.c(),
        }
    }
    fn rust(&self, ty: Type) -> String {
        match ty {
            Type::Int => "i64".into(),
            Type::Number(n) => n.name().into(),
            Type::Float(n) => n.name().into(),
            Type::Bool => "bool".into(),
            Type::Unit => "()".into(),
            Type::Fixed(n, count) => format!("Buffer_{}_{count}", n.name()),
            Type::Named(id) => {
                format!("Record_{}", ident(&self.program.types.definitions[id].name))
            }
            Type::Id(id) => format!("Handle_{}", ident(&self.program.types.definitions[id].name)),
            _ => unreachable!(),
        }
    }
    fn wire_rust(&self, ty: Type) -> String {
        if ty == Type::Bool {
            "u8".into()
        } else {
            self.rust(ty)
        }
    }
    fn ordered(&self) -> Vec<Type> {
        self.types
            .iter()
            .copied()
            .filter(|t| matches!(t, Type::Fixed(..)))
            .chain(
                self.types
                    .iter()
                    .copied()
                    .filter(|t| matches!(t, Type::Id(_))),
            )
            .chain(
                self.program
                    .types
                    .order
                    .iter()
                    .map(|id| Type::Named(*id))
                    .filter(|t| self.types.contains(t)),
            )
            .collect()
    }
    fn declarations(&self, link_name: &str) -> (String, String, String) {
        let prefix = self.prefix;
        let guard = format!("{prefix}_ABI_H");
        let mut header = format!("/* Generated Anode C ABI v1. Windows x64, MSVC __cdecl, pack 8.\n   int/i64 are signed 64-bit; bool is a byte (nonzero input, canonical 0/1 output).\n   Aggregate value inputs are copied. Borrowed pointers are valid/aligned for\n   the call; mutable arguments and result storage must not overlap any argument.\n   Records/buffers own no heap memory. No pointer is retained.\n   Contract/arithmetic/runtime failures terminate the host process. Link the\n   supplied .lib with the MSVC dynamic CRT (/MD) and kernel32.lib. */\n#ifndef {guard}\n#define {guard}\n#include <stdint.h>\n#include <stddef.h>\n#if !defined(_WIN64) || !defined(_M_X64)\n#error Anode library ABI requires Windows x64\n#endif\n#ifdef __cplusplus\n#define {prefix}_ASSERT static_assert\nextern \"C\" {{\n#else\n#define {prefix}_ASSERT _Static_assert\n#endif\n#define {prefix}_CALL __cdecl\n#pragma pack(push, 8)\n{prefix}_ASSERT(sizeof(void*) == 8, \"pointer ABI\");\n{prefix}_ASSERT(sizeof(float) == 4 && sizeof(double) == 8, \"float ABI\");\n");
        let mut rust = format!("// Generated safe bindings for {link_name}.lib, Anode C ABI v1.\n// Import this file as a module and pass its directory to rustc -L native=... .\n// No heap ownership or retained pointers cross the ABI. Borrowed POD values\n// are copied in/out; Rust references enforce the required non-aliasing.\n// Contract/arithmetic/runtime failures terminate the host process.\n#![allow(non_camel_case_types, non_snake_case, dead_code)]\n#[cfg(not(all(target_os = \"windows\", target_arch = \"x86_64\")))]\ncompile_error!(\"Anode library ABI requires Windows x64\");\n");
        let mut conversion = String::new();
        for ty in self.ordered() {
            let c = self.c(ty);
            let r = self.rust(ty);
            let internal = ty.c();
            let layout = self.program.types.layout(ty);
            match ty {
                Type::Id(_) => {
                    header.push_str(&format!("/* Nominal instance-relative slot/generation bits; every arena lookup validates them. */\ntypedef struct {{ uint64_t opaque_bits; }} {c};\n"));
                    rust.push_str(&format!("#[repr(transparent)]\n#[derive(Clone,Copy,Debug,Default,PartialEq,Eq,Hash)]\npub struct {r} {{ bits:u64 }}\nimpl {r} {{ /// Wraps instance-relative raw bits; this does not establish liveness. Arena operations always validate.\n pub const fn from_bits(bits:u64)->Self{{Self{{bits}}}} pub const fn bits(self)->u64{{self.bits}} }}\n"));
                    conversion.push_str(&format!("static void {c}_read({internal}* out,const {c}* in){{*out=in->opaque_bits;}}\nstatic void {c}_write({c}* out,const {internal}* in){{out->opaque_bits=*in;}}\n"));
                }
                Type::Fixed(n, count) => {
                    header.push_str(&format!(
                        "typedef struct {{ {} data[{count}]; }} {c};\n",
                        n.c()
                    ));
                    rust.push_str(&format!("#[repr(C)]\n#[derive(Clone, Copy, Debug, PartialEq)]\npub struct {r} {{ pub data: [{}; {count}] }}\nimpl Default for {r} {{ fn default() -> Self {{ Self {{ data: [0; {count}] }} }} }}\n", n.name()));
                    conversion.push_str(&format!("static void {c}_read({internal}* out,const {c}* in){{memcpy(out->data,in->data,sizeof(out->data));}}\nstatic void {c}_write({c}* out,const {internal}* in){{memcpy(out->data,in->data,sizeof(out->data));}}\n"));
                }
                Type::Named(id) => {
                    let DataKind::Record(fields) = &self.program.types.definitions[id].kind else {
                        unreachable!()
                    };
                    header.push_str("typedef struct {\n");
                    rust.push_str(&format!(
                        "#[repr(C)]\n#[derive(Clone, Copy, Debug, PartialEq)]\npub struct {r} {{\n"
                    ));
                    let mut read = format!("static void {c}_read({internal}* out,const {c}* in){{memset(out,0,sizeof(*out));\n");
                    let mut write = format!("static void {c}_write({c}* out,const {internal}* in){{memset(out,0,sizeof(*out));\n");
                    let mut defaults = String::new();
                    for (index, field) in fields.iter().enumerate() {
                        let name = format!("f_{}", ident(&field.name));
                        header.push_str(&format!("    {} {name};\n", self.c(field.ty)));
                        rust.push_str(&format!("    pub {name}: {},\n", self.rust(field.ty)));
                        defaults.push_str(&format!("{name}: Default::default(),"));
                        if aggregate(field.ty) {
                            read.push_str(&format!(
                                "{}_read(&out->f{index},&in->{name});\n",
                                self.c(field.ty)
                            ));
                            write.push_str(&format!(
                                "{}_write(&out->{name},&in->f{index});\n",
                                self.c(field.ty)
                            ));
                        } else {
                            let normalize = if field.ty == Type::Bool { " != 0" } else { "" };
                            read.push_str(&format!("out->f{index}=in->{name}{normalize};\n"));
                            write.push_str(&format!("out->{name}=in->f{index}{normalize};\n"));
                        }
                    }
                    header.push_str(&format!("}} {c};\n"));
                    rust.push_str(&format!("}}\nimpl Default for {r} {{ fn default() -> Self {{ Self {{ {defaults} }} }} }}\n"));
                    read.push_str("}\n");
                    write.push_str("}\n");
                    conversion.push_str(&read);
                    conversion.push_str(&write);
                    for (index, field) in fields.iter().enumerate() {
                        let name = format!("f_{}", ident(&field.name));
                        let offset = layout.offsets[index];
                        header.push_str(&format!("{prefix}_ASSERT(offsetof({c}, {name}) == {offset}, \"field offset ABI\");\n"));
                        rust.push_str(&format!(
                            "const _: [(); {offset}] = [(); core::mem::offset_of!({r}, {name})];\n"
                        ));
                    }
                }
                _ => unreachable!(),
            }
            header.push_str(&format!("{prefix}_ASSERT(sizeof({c}) == {}, \"size ABI\");\n{prefix}_ASSERT(__alignof({c}) == {}, \"alignment ABI\");\n", layout.size, layout.align));
            rust.push_str(&format!("const _: [(); {}] = [(); core::mem::size_of::<{r}>()];\nconst _: [(); {}] = [(); core::mem::align_of::<{r}>()];\n", layout.size, layout.align));
        }
        (header, rust, conversion)
    }
    #[allow(clippy::too_many_arguments)]
    fn function(
        &self,
        f: &Function,
        export: &Export,
        index: usize,
        header: &mut String,
        shim: &mut String,
        raw: &mut String,
        safe: &mut String,
    ) {
        let mut c_params = Vec::new();
        let mut raw_params = Vec::new();
        let mut safe_params = Vec::new();
        let mut c_args = Vec::new();
        let mut rust_args = Vec::new();
        let mut before = String::new();
        let mut after = String::new();
        let mut rust_before = String::new();
        let mut rust_after = String::new();
        for (i, p) in f.params.iter().enumerate() {
            let c = self.c(p.ty);
            let r = self.rust(p.ty);
            let wire = self.wire_rust(p.ty);
            let by_pointer = aggregate(p.ty) || p.borrow != Borrow::Owned;
            let mutable = p.borrow == Borrow::Exclusive;
            let qualifier = if mutable { "" } else { "const " };
            c_params.push(if by_pointer {
                format!("{qualifier}{c}* a{i}")
            } else {
                format!("{c} a{i}")
            });
            raw_params.push(if by_pointer {
                format!("a{i}: *{} {wire}", if mutable { "mut" } else { "const" })
            } else {
                format!("a{i}: {wire}")
            });
            safe_params.push(format!(
                "a{i}: {}{r}",
                match p.borrow {
                    Borrow::Owned => "",
                    Borrow::Shared => "&",
                    Borrow::Exclusive => "&mut ",
                }
            ));
            if aggregate(p.ty) {
                before.push_str(&format!("{} v{i}; {c}_read(&v{i},a{i});\n", p.ty.c()));
                c_args.push(if f.external_symbol.is_some() {
                    format!("v{i}.data")
                } else {
                    format!("{}v{i}", if p.borrow == Borrow::Owned { "" } else { "&" })
                });
                if mutable {
                    after.push_str(&format!("{c}_write(a{i},&v{i});\n"));
                }
                rust_args.push(if p.borrow == Borrow::Owned {
                    format!("&a{i}")
                } else {
                    format!("a{i}")
                });
            } else if p.borrow != Borrow::Owned {
                let normalize = if p.ty == Type::Bool { " != 0" } else { "" };
                before.push_str(&format!("{} v{i}=*a{i}{normalize};\n", p.ty.c()));
                c_args.push(format!("&v{i}"));
                if mutable {
                    after.push_str(&format!("*a{i}=v{i}{normalize};\n"));
                }
                if p.ty == Type::Bool {
                    rust_before.push_str(&format!(
                        "let {}b{i} = u8::from(*a{i});\n",
                        if mutable { "mut " } else { "" }
                    ));
                    rust_args.push(format!("&{}b{i}", if mutable { "mut " } else { "" }));
                    if mutable {
                        rust_after.push_str(&format!("*a{i} = b{i} != 0;\n"));
                    }
                } else {
                    rust_args.push(format!("a{i}"));
                }
            } else {
                c_args.push(format!(
                    "a{i}{}",
                    if p.ty == Type::Bool { " != 0" } else { "" }
                ));
                rust_args.push(if p.ty == Type::Bool {
                    format!("u8::from(a{i})")
                } else {
                    format!("a{i}")
                });
            }
        }
        let aggregate_result = aggregate(f.ret);
        if aggregate_result {
            c_params.push(format!("{}* out", self.c(f.ret)));
            raw_params.push(format!("out: *mut {}", self.rust(f.ret)));
            rust_before.push_str(&format!(
                "let mut out = core::mem::MaybeUninit::<{}>::uninit();\n",
                self.rust(f.ret)
            ));
            rust_args.push("out.as_mut_ptr()".into());
        }
        let c_ret = if aggregate_result {
            "void".into()
        } else {
            self.c(f.ret)
        };
        let raw_ret = if aggregate_result {
            "()".into()
        } else {
            self.wire_rust(f.ret)
        };
        let c_signature = format!(
            "{c_ret} __cdecl {}({})",
            export.symbol,
            if c_params.is_empty() {
                "void".into()
            } else {
                c_params.join(", ")
            }
        );
        header.push_str(&format!(
            "/* {} */\n{c_ret} {}_CALL {}({});\n",
            export.source_name,
            self.prefix,
            export.symbol,
            if c_params.is_empty() {
                "void".into()
            } else {
                c_params.join(", ")
            }
        ));
        shim.push_str(&format!("{c_signature} {{\n{before}"));
        let target = f
            .external_symbol
            .clone()
            .unwrap_or_else(|| format!("zn_fn_{}", f.name));
        let external_buffer_result = f.external_symbol.is_some() && aggregate_result;
        if external_buffer_result {
            c_args.insert(0, "result.data".into());
        }
        let call = format!("{target}({})", c_args.join(", "));
        if f.ret == Type::Unit {
            shim.push_str(&format!("{call};\n{after}"));
        } else {
            if external_buffer_result {
                shim.push_str(&format!("{} result; {call};\n{after}", f.ret.c()));
            } else {
                shim.push_str(&format!("{} result={call};\n{after}", f.ret.c()));
            }
            if aggregate_result {
                shim.push_str(&format!("{}_write(out,&result);\n", self.c(f.ret)));
            } else {
                shim.push_str(&format!(
                    "return result{};\n",
                    if f.ret == Type::Bool { " != 0" } else { "" }
                ));
            }
        }
        shim.push_str("}\n");
        raw.push_str(&format!(
            "    #[link_name = {:?}]\n    fn raw_{index}({}) -> {raw_ret};\n",
            export.symbol,
            raw_params.join(", ")
        ));
        safe.push_str(&format!("/// Calls Anode `{}` with checked POD arguments.\npub fn {}({}) -> {} {{\n{rust_before}", export.source_name, export.rust_name, safe_params.join(", "), self.rust(f.ret)));
        safe.push_str(&format!(
            "let result = unsafe {{ raw_{index}({}) }};\n{rust_after}",
            rust_args.join(", ")
        ));
        if aggregate_result {
            safe.push_str("let _ = result;\nunsafe { out.assume_init() }\n");
        } else if f.ret == Type::Bool {
            safe.push_str("result != 0\n");
        } else {
            safe.push_str("result\n");
        }
        safe.push_str("}\n");
    }
}
