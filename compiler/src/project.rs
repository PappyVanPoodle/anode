//! File loading, module resolution and the deliberately small project format.
use crate::{checker, syntax::*, Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// Largest accepted source file, and all modules of a project together, in bytes.
pub const SOURCE_FILE_LIMIT: usize = 16 * 1024 * 1024;
pub const PROJECT_SOURCE_LIMIT: usize = 256 * 1024 * 1024;

pub struct SourceFile {
    pub path: PathBuf,
    pub source: String,
    /// Stable snapshot/overlay key. Dependency files live below
    /// `@dependencies/<alias path>/`, never an absolute machine path.
    pub source_key: String,
    /// Canonical dependency namespace; the entry package uses the empty string.
    pub package: String,
    pub(crate) offset: usize,
    pub(crate) module: String,
    pub(crate) parsed: Arc<Module>,
    // `source` is public for historical callers. Never treat a mutated source
    // buffer as the bytes from which the immutable syntax was parsed.
    parsed_source: Arc<str>,
}
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub managed: bool,
    pub files: Vec<SourceFile>,
    pub manifest: String,
    pub cache_bytes: u64,
    capabilities: Option<BTreeSet<String>>,
    library_exports: Option<BTreeSet<String>>,
    packages: BTreeMap<String, Package>,
}
#[derive(Clone)]
struct Package {
    root: PathBuf,
    manifest: String,
    entry: PathBuf,
    dependencies: BTreeMap<String, String>,
    allowed: Option<BTreeSet<String>>,
    identity: String,
}
/// Request-local syntax only. Borrowing the source snapshot pins every path,
/// module/package identity, manifest and parser environment for its lifetime.
/// Parser type IDs are local to exact module bytes; resolution is never cached.
pub(crate) struct ParseCache<'a> {
    project: &'a Project,
    versions: BTreeMap<usize, BTreeMap<String, Arc<Module>>>,
    holes: BTreeMap<usize, BTreeMap<String, Vec<crate::navigation::HoleSyntax>>>,
    retained_bytes: usize,
}
impl<'a> ParseCache<'a> {
    pub(crate) fn new(project: &'a Project) -> Self {
        Self {
            project,
            versions: BTreeMap::new(),
            holes: BTreeMap::new(),
            retained_bytes: 0,
        }
    }
    fn parse(&mut self, index: usize, source: &str) -> Result<Arc<Module>> {
        let file = &self.project.files[index];
        if let Some(parsed) = self
            .versions
            .get(&index)
            .and_then(|versions| versions.get(source))
        {
            profile_work(|profile| profile.syntax_reuses += 1);
            return Ok(Arc::clone(parsed));
        }
        let parsed = Arc::new(parse_module(source).map_err(|e| e.in_file(&file.path))?);
        // Candidate syntax can be large. Keep reuse bounded to this request;
        // exhausting the budget just reparses normally, never omits a check.
        if self.retained_bytes.saturating_add(source.len()) <= 8 * 1024 * 1024 {
            self.retained_bytes += source.len();
            self.versions
                .entry(index)
                .or_default()
                .insert(source.into(), Arc::clone(&parsed));
        } else {
            profile_work(|profile| profile.cache_budget_fallbacks += 1);
        }
        Ok(parsed)
    }
    pub(crate) fn hole_syntax(
        &mut self,
        project: &Project,
    ) -> Result<Vec<crate::navigation::HoleSyntax>> {
        if project.root != self.project.root
            || project.manifest != self.project.manifest
            || project.files.len() != self.project.files.len()
            || project
                .files
                .iter()
                .zip(&self.project.files)
                .any(|(file, original)| {
                    file.path != original.path
                        || file.source_key != original.source_key
                        || file.package != original.package
                        || file.module != original.module
                })
        {
            return Err(Error::new(
                0,
                "hole syntax cache belongs to another project snapshot",
            ));
        }
        let mut result = Vec::new();
        for (index, file) in project.files.iter().enumerate() {
            if let Some(holes) = self
                .holes
                .get(&index)
                .and_then(|versions| versions.get(&file.source))
            {
                profile_work(|profile| profile.syntax_reuses += 1);
                result.extend_from_slice(holes);
            } else {
                let holes = crate::navigation::file_hole_syntax(index, file)?;
                let weight = holes.iter().fold(file.source.len(), |weight, hole| {
                    let text = hole
                        .requires
                        .iter()
                        .chain(&hole.ensures)
                        .fold(0usize, |n, text| n.saturating_add(text.len()));
                    weight
                        .saturating_add(std::mem::size_of::<crate::navigation::HoleSyntax>())
                        .saturating_add(hole.function.len())
                        .saturating_add(hole.expected_type.len())
                        .saturating_add(hole.intent.len())
                        .saturating_add(text)
                });
                if self.retained_bytes.saturating_add(weight) <= 8 * 1024 * 1024 {
                    self.retained_bytes += weight;
                    self.holes
                        .entry(index)
                        .or_default()
                        .insert(file.source.clone(), holes.clone());
                } else {
                    profile_work(|profile| profile.cache_budget_fallbacks += 1);
                }
                result.extend(holes);
            }
        }
        Ok(result)
    }
}
pub struct CompiledProject {
    pub c: String,
    pub units: Vec<(String, String)>,
    pub tests: usize,
}
struct Manifest {
    name: String,
    entry: PathBuf,
    cache_bytes: u64,
    capabilities: Option<BTreeSet<String>>,
    library_exports: Option<BTreeSet<String>>,
    dependencies: BTreeMap<String, PathBuf>,
}

fn io(path: &Path, error: std::io::Error) -> Error {
    Error::new(0, error.to_string()).in_file(path)
}
fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && ![
            "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7",
            "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
        ]
        .contains(&name.to_ascii_lowercase().as_str())
}
fn manifest(source: &str) -> Result<Manifest> {
    let mut section = "";
    let mut values = BTreeMap::new();
    let mut capabilities = None;
    let mut dependencies = BTreeMap::new();
    for (line, raw) in source.lines().enumerate() {
        // A # inside a quoted string is data, not a comment.
        let mut quoted = false;
        let end = raw
            .char_indices()
            .find_map(|(i, c)| {
                if c == '"' {
                    quoted = !quoted;
                }
                (c == '#' && !quoted).then_some(i)
            })
            .unwrap_or(raw.len());
        let text = raw[..end].trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with('[') {
            section = match text {
                "[package]" => "package",
                "[build]" => "build",
                "[dependencies]" => "dependencies",
                "[capabilities]" => {
                    capabilities.get_or_insert_with(BTreeSet::new);
                    "capabilities"
                }
                _ => {
                    return Err(Error::new(
                        line + 1,
                        "supported manifest sections: [package], [build], [capabilities], [dependencies]",
                    ))
                }
            };
            continue;
        }
        let (key, value) = text
            .split_once('=')
            .ok_or_else(|| Error::new(line + 1, "expected key = \"value\""))?;
        let key = key.trim();
        if section == "dependencies" {
            if !source_identifier(key) || key == "crate" {
                return Err(Error::new(
                    line + 1,
                    "dependency name must be an identifier other than crate",
                ));
            }
            let value = value.trim();
            let path = if value.starts_with('{') {
                let body = value
                    .strip_prefix('{')
                    .and_then(|v| v.strip_suffix('}'))
                    .ok_or_else(|| {
                        Error::new(line + 1, "dependency requires { path = \"directory\" }")
                    })?
                    .trim();
                let (field, path) = body.split_once('=').ok_or_else(|| {
                    Error::new(line + 1, "dependency requires { path = \"directory\" }")
                })?;
                if field.trim() != "path" {
                    return Err(Error::new(
                        line + 1,
                        "only local dependency path is supported",
                    ));
                }
                path.trim()
            } else {
                value
            };
            let path = quoted_string(path, line + 1)?;
            if path.is_empty() {
                return Err(Error::new(line + 1, "dependency path must not be empty"));
            }
            if dependencies
                .insert(key.to_owned(), PathBuf::from(path))
                .is_some()
            {
                return Err(Error::new(line + 1, "duplicate dependency name"));
            }
            continue;
        }
        let key = format!("{section}.{key}");
        if ![
            "package.name",
            "package.version",
            "build.entry",
            "build.cache_mb",
            "build.exports",
            "capabilities.file_read",
            "capabilities.file_write",
            "capabilities.process_args",
            "capabilities.process",
            "capabilities.threads",
            "capabilities.ffi",
            "capabilities.net",
        ]
        .contains(&key.as_str())
        {
            return Err(Error::new(
                line + 1,
                format!("unsupported manifest key {key}"),
            ));
        }
        if let Some(capability) = key.strip_prefix("capabilities.") {
            let enabled = match value.trim() {
                "true" => true,
                "false" => false,
                _ => return Err(Error::new(line + 1, "capabilities require true or false")),
            };
            if enabled {
                capabilities.as_mut().unwrap().insert(capability.to_owned());
            }
            if values.insert(key, value.trim().to_owned()).is_some() {
                return Err(Error::new(line + 1, "duplicate manifest key"));
            }
            continue;
        }
        if key == "build.cache_mb" {
            let amount: u64 = value
                .trim()
                .parse()
                .map_err(|_| Error::new(line + 1, "cache_mb must be an integer from 0 to 4096"))?;
            if amount > 4096 {
                return Err(Error::new(line + 1, "cache_mb limit is 4096 MiB"));
            }
            if values.insert(key, amount.to_string()).is_some() {
                return Err(Error::new(line + 1, "duplicate manifest key"));
            }
            continue;
        }
        let value = quoted_string(value.trim(), line + 1)?;
        if values.insert(key, value.to_string()).is_some() {
            return Err(Error::new(line + 1, "duplicate manifest key"));
        }
    }
    let name = values
        .remove("package.name")
        .ok_or_else(|| Error::new(1, "missing package.name"))?;
    if !identifier(&name) {
        return Err(Error::new(
            1,
            "package name must be a non-reserved filename using letters, digits, - or _",
        ));
    }
    let entry = PathBuf::from(
        values
            .remove("build.entry")
            .unwrap_or_else(|| "src/main.zn".into()),
    );
    if entry
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
        || entry.extension().is_none_or(|e| e != "zn" && e != "an")
    {
        return Err(Error::new(
            1,
            "build.entry must be a relative .an or .zn path inside the project",
        ));
    }
    let cache_bytes = values
        .remove("build.cache_mb")
        .map_or(0, |s| s.parse::<u64>().unwrap() * 1024 * 1024);
    let library_exports = values
        .remove("build.exports")
        .map(|names| {
            let mut exports = BTreeSet::new();
            for name in names.split(',').map(str::trim) {
                if !name.split("::").all(source_identifier) {
                    return Err(Error::new(
                        1,
                        "build.exports requires comma-separated qualified function names",
                    ));
                }
                if !exports.insert(name.to_owned()) {
                    return Err(Error::new(1, "duplicate build.exports function"));
                }
            }
            Ok(exports)
        })
        .transpose()?;
    Ok(Manifest {
        name,
        entry,
        cache_bytes,
        capabilities,
        library_exports,
        dependencies,
    })
}

fn source_identifier(name: &str) -> bool {
    name.bytes()
        .enumerate()
        .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        && !name.is_empty()
}
fn quoted_string(value: &str, line: usize) -> Result<&str> {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .filter(|v| !v.contains(['"', '\\']) && !v.chars().any(char::is_control))
        .ok_or_else(|| Error::new(line, "use a quoted string; paths use forward slashes"))
}
fn read_manifest(path: &Path) -> Result<(Manifest, String)> {
    let text = fs::read_to_string(path).map_err(|e| io(path, e))?;
    if text.len() > 16384 {
        return Err(Error::new(0, "manifest limit is 16 KiB").in_file(path));
    }
    let mut parsed = manifest(&text).map_err(|e| e.in_file(path))?;
    if path.ends_with("Anode.toml") && !text.lines().any(|l| l.trim().starts_with("entry")) {
        parsed.entry = PathBuf::from("src/main.an");
    }
    Ok((parsed, text))
}
fn select_manifest(directory: &Path) -> Result<PathBuf> {
    let anode = directory.join("Anode.toml");
    let legacy = directory.join("Zenith.toml");
    match (anode.is_file(), legacy.is_file()) {
        (true, true) => Err(Error::new(
            0,
            "both Anode.toml and Zenith.toml exist; specify a manifest explicitly",
        )
        .in_file(directory)),
        (true, false) => Ok(anode),
        (false, true) => Ok(legacy),
        (false, false) => Err(Error::new(
            0,
            "missing dependency/project manifest: expected Anode.toml or Zenith.toml",
        )
        .in_file(anode)),
    }
}

impl Project {
    pub fn from_source(source: &str) -> Result<Self> {
        let parsed = parse_module(source)?;
        if !parsed.modules.is_empty() {
            return Err(Error::new(
                parsed.modules[0].line,
                "modules require a file/project build",
            ));
        }
        Ok(Self {
            root: PathBuf::from("."),
            name: "memory".into(),
            managed: false,
            manifest: String::new(),
            cache_bytes: 0,
            capabilities: None,
            library_exports: None,
            packages: BTreeMap::from([(
                String::new(),
                Package {
                    root: PathBuf::from("."),
                    manifest: String::new(),
                    entry: PathBuf::from("<source>"),
                    dependencies: BTreeMap::new(),
                    allowed: None,
                    identity: "memory".into(),
                },
            )]),
            files: vec![SourceFile {
                path: PathBuf::from("<source>"),
                source: source.into(),
                source_key: "<source>".into(),
                package: String::new(),
                offset: 0,
                module: String::new(),
                parsed: Arc::new(parsed),
                parsed_source: Arc::from(source),
            }],
        })
    }
    pub fn load(input: &Path) -> Result<Self> {
        Self::open(input, true)
    }
    pub fn source_key(&self, path: &Path) -> Option<&str> {
        self.files
            .iter()
            .find(|f| f.path == path)
            .map(|f| f.source_key.as_str())
    }
    pub fn manifest_paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.packages.values().flat_map(|package| {
            [
                package.root.join("Anode.toml"),
                package.root.join("Zenith.toml"),
            ]
        })
    }
    /// The effective authority for a particular loaded source. Unmanaged/root
    /// projects retain historical authority except for FFI and subprocesses,
    /// which always need explicit permission.
    pub fn source_capabilities(&self, path: &Path) -> Option<&BTreeSet<String>> {
        self.files
            .iter()
            .find(|f| f.path == path)
            .and_then(|f| self.packages[&f.package].allowed.as_ref())
    }
    /// An optional explicit ABI surface. Language-level `pub` remains module
    /// visibility; all source bodies are checked even when not ABI exports.
    /// Absent this setting, historical export-all-public behavior is retained.
    pub fn library_exports(&self) -> Option<&BTreeSet<String>> {
        self.library_exports.as_ref()
    }
    pub(crate) fn qualified_for(&self, file: &SourceFile, name: &str) -> String {
        if let Some(local) = name.strip_prefix("crate::") {
            return qualified(&file.package, local);
        }
        if let Some((alias, rest)) = name.split_once("::") {
            if let Some(namespace) = self.packages[&file.package].dependencies.get(alias) {
                return format!("{namespace}::{rest}");
            }
        }
        qualified(&file.module, name)
    }
    /// Speculative edits are confined to already-loaded files. Module graph and
    /// manifest/capabilities are fixed for the session; reload to change those.
    pub fn with_overlays(&self, overlays: &BTreeMap<String, String>) -> Result<Self> {
        self.overlay_snapshot(overlays, None)
    }
    pub(crate) fn syntax_current(&self) -> bool {
        self.files
            .iter()
            .all(|file| file.source == file.parsed_source.as_ref())
    }
    pub(crate) fn with_overlays_cached(
        &self,
        overlays: &BTreeMap<String, String>,
        cache: &mut ParseCache<'_>,
    ) -> Result<Self> {
        // This internal cache cannot be carried to a different snapshot, even
        // when paths happen to agree. Exact source equality is checked below.
        if !std::ptr::eq(self, cache.project) {
            return Err(Error::new(
                0,
                "syntax cache belongs to another project snapshot",
            ));
        }
        self.overlay_snapshot(overlays, Some(cache))
    }
    fn overlay_snapshot(
        &self,
        overlays: &BTreeMap<String, String>,
        mut cache: Option<&mut ParseCache<'_>>,
    ) -> Result<Self> {
        let mut used = BTreeSet::new();
        let mut files: Vec<SourceFile> = Vec::with_capacity(self.files.len());
        let mut total = 0;
        let mut physical = BTreeMap::<&Path, &String>::new();
        for f in &self.files {
            if let Some(source) = overlays.get(&f.source_key) {
                if physical
                    .insert(&f.path, source)
                    .is_some_and(|old| old != source)
                {
                    return Err(
                        Error::new(0, "overlays for shared dependency sources must agree")
                            .in_file(&f.path),
                    );
                }
            }
        }
        for (index, f) in self.files.iter().enumerate() {
            let source = if let Some(s) = overlays.get(&f.source_key) {
                used.insert(f.source_key.clone());
                s.clone()
            } else if let Some(s) = physical.get(f.path.as_path()) {
                (*s).clone()
            } else {
                f.source.clone()
            };
            total += source.len();
            if source.len() > SOURCE_FILE_LIMIT || total > PROJECT_SOURCE_LIMIT {
                return Err(Error::new(0, "overlay exceeds source size limits"));
            }
            let unchanged = source == f.parsed_source.as_ref();
            let parsed = if unchanged {
                profile_work(|profile| profile.syntax_reuses += 1);
                Arc::clone(&f.parsed)
            } else if let Some(cache) = cache.as_deref_mut() {
                cache.parse(index, &source)?
            } else {
                Arc::new(parse_module(&source).map_err(|e| e.in_file(&f.path))?)
            };
            if !parsed
                .modules
                .iter()
                .map(|m| &m.name)
                .eq(f.parsed.modules.iter().map(|m| &m.name))
            {
                return Err(Error::new(
                    0,
                    "overlay cannot change the module graph; reload the project",
                )
                .in_file(&f.path));
            }
            let offset = files
                .last()
                .map_or(0, |prev| prev.offset + prev.source.lines().count() + 2);
            files.push(SourceFile {
                path: f.path.clone(),
                parsed_source: if unchanged {
                    Arc::clone(&f.parsed_source)
                } else {
                    Arc::from(source.as_str())
                },
                source,
                source_key: f.source_key.clone(),
                package: f.package.clone(),
                offset,
                module: f.module.clone(),
                parsed,
            });
        }
        if used.len() != overlays.len() {
            return Err(Error::new(
                0,
                "overlay paths must exactly match loaded project-relative paths (forward slashes)",
            ));
        }
        Ok(Self {
            root: self.root.clone(),
            name: self.name.clone(),
            managed: self.managed,
            files,
            manifest: self.manifest.clone(),
            cache_bytes: self.cache_bytes,
            capabilities: self.capabilities.clone(),
            library_exports: self.library_exports.clone(),
            packages: self.packages.clone(),
        })
    }
    pub fn layout(input: &Path) -> Result<Self> {
        Self::open(input, false)
    }
    fn open(input: &Path, load_sources: bool) -> Result<Self> {
        let input = input.canonicalize().map_err(|e| io(input, e))?;
        let managed = input.is_dir()
            || input
                .file_name()
                .is_some_and(|n| n == "Zenith.toml" || n == "Anode.toml");
        let root = if input.is_dir() {
            input.clone()
        } else {
            input.parent().unwrap().to_owned()
        };
        let (
            name,
            entry,
            manifest_text,
            cache_bytes,
            capabilities,
            library_exports,
            dependencies,
            manifest_path,
        ) = if managed {
            let path = if input.is_file() {
                input.clone()
            } else {
                select_manifest(&root)?
            };
            let (manifest, text) = read_manifest(&path)?;
            (
                manifest.name,
                root.join(manifest.entry),
                text,
                manifest.cache_bytes,
                manifest.capabilities,
                manifest.library_exports,
                manifest.dependencies,
                Some(path),
            )
        } else {
            if input.extension().is_none_or(|e| e != "zn" && e != "an") {
                return Err(Error::new(0, "expected an .an/.zn file or Anode project"));
            }
            (
                input.file_stem().unwrap().to_string_lossy().into_owned(),
                input,
                String::new(),
                0,
                None,
                None,
                BTreeMap::new(),
                None,
            )
        };
        let mut project = Self {
            root,
            name,
            managed,
            files: Vec::new(),
            manifest: manifest_text,
            cache_bytes,
            capabilities: capabilities.clone(),
            library_exports,
            packages: BTreeMap::new(),
        };
        let identity = crate::backend::sha256(
            format!(
                "root\n{}\n{}\n{}",
                project.root.display(),
                project.name,
                project.manifest
            )
            .as_bytes(),
        );
        project.packages.insert(
            String::new(),
            Package {
                root: project.root.clone(),
                manifest: project.manifest.clone(),
                entry: entry.clone(),
                dependencies: BTreeMap::new(),
                allowed: capabilities,
                identity,
            },
        );
        let mut stack = manifest_path.into_iter().collect();
        project.load_dependencies("", dependencies, &mut stack)?;
        if project.packages.len() > 1 {
            for (namespace, package) in &project.packages {
                if !namespace.is_empty() {
                    project.manifest.push_str(&format!(
                        "\n# dependency {namespace}\n# identity {}\n{}",
                        package.identity, package.manifest
                    ));
                }
            }
        }
        if load_sources {
            let entries: Vec<_> = project
                .packages
                .iter()
                .map(|(namespace, package)| (namespace.clone(), package.entry.clone()))
                .collect();
            for (namespace, entry) in entries {
                project.load_module(&entry, &namespace, &namespace, 0)?;
            }
        }
        Ok(project)
    }
    fn load_dependencies(
        &mut self,
        parent: &str,
        dependencies: BTreeMap<String, PathBuf>,
        stack: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for (alias, relative) in dependencies {
            let namespace = qualified(parent, &alias);
            if stack.len() > 32 || self.packages.len() >= 128 {
                return Err(Error::new(0, "dependency limit: depth 32, 128 packages"));
            }
            let path = self.packages[parent].root.join(relative);
            let manifest_path = if path.is_file()
                && path
                    .file_name()
                    .is_some_and(|n| n == "Anode.toml" || n == "Zenith.toml")
            {
                path
            } else {
                select_manifest(&path)?
            };
            let manifest_path = manifest_path
                .canonicalize()
                .map_err(|e| io(&manifest_path, e))?;
            if stack.contains(&manifest_path) {
                return Err(Error::new(0, format!("dependency cycle at {namespace}"))
                    .in_file(manifest_path));
            }
            let (manifest, text) = read_manifest(&manifest_path)?;
            let root = manifest_path.parent().unwrap().to_owned();
            let mut allowed = manifest.capabilities.clone().unwrap_or_default();
            if let Some(inherited) = &self.packages[parent].allowed {
                allowed.retain(|c| inherited.contains(c));
            } else {
                // Historical authority never implicitly includes FFI/processes
                // or networking.
                allowed.remove("ffi");
                allowed.remove("process");
                allowed.remove("net");
            }
            let identity = crate::backend::sha256(
                format!(
                    "{namespace}\n{}\n{}\n{text}\n{allowed:?}",
                    manifest_path.display(),
                    manifest.name
                )
                .as_bytes(),
            );
            self.packages
                .get_mut(parent)
                .unwrap()
                .dependencies
                .insert(alias, namespace.clone());
            self.packages.insert(
                namespace.clone(),
                Package {
                    root: root.clone(),
                    manifest: text,
                    entry: root.join(manifest.entry),
                    dependencies: BTreeMap::new(),
                    allowed: Some(allowed),
                    identity,
                },
            );
            stack.push(manifest_path);
            self.load_dependencies(&namespace, manifest.dependencies, stack)?;
            stack.pop();
        }
        Ok(())
    }
    fn load_module(
        &mut self,
        path: &Path,
        module: &str,
        package: &str,
        depth: usize,
    ) -> Result<()> {
        let canonical = path.canonicalize().map_err(|e| io(path, e))?;
        let package_root = &self.packages[package].root;
        if !canonical.starts_with(package_root) {
            return Err(Error::new(0, "module resolves outside project root").in_file(path));
        }
        if depth > 32 || self.files.len() >= 128 {
            return Err(Error::new(0, "module limit: depth 32, 128 files").in_file(path));
        }
        if self
            .files
            .iter()
            .any(|f| f.path == canonical && f.package == package)
        {
            return Err(
                Error::new(0, "module cycle or duplicate physical source file").in_file(path),
            );
        }
        if fs::metadata(&canonical).map_err(|e| io(path, e))?.len() > SOURCE_FILE_LIMIT as u64 {
            return Err(Error::new(0, "source file limit is 16 MiB").in_file(path));
        }
        let source = fs::read_to_string(&canonical).map_err(|e| io(path, e))?;
        if self.files.iter().map(|f| f.source.len()).sum::<usize>() + source.len() > PROJECT_SOURCE_LIMIT
        {
            return Err(Error::new(0, "project source limit is 256 MiB").in_file(path));
        }
        let parsed = parse_module(&source).map_err(|e| e.in_file(path))?;
        let mut declared = BTreeSet::new();
        for decl in &parsed.modules {
            if !declared.insert(decl.name.clone()) {
                return Err(Error::new(decl.line, "duplicate module declaration").in_file(path));
            }
        }
        let children = parsed.modules.clone();
        for decl in &children {
            if module == package && self.packages[package].dependencies.contains_key(&decl.name) {
                return Err(
                    Error::new(decl.line, "module name conflicts with a dependency alias")
                        .in_file(path),
                );
            }
        }
        let offset = self
            .files
            .last()
            .map_or(0, |f| f.offset + f.source.lines().count() + 2);
        self.files.push(SourceFile {
            path: canonical.clone(),
            parsed_source: Arc::from(source.as_str()),
            source,
            source_key: if package.is_empty() {
                canonical
                    .strip_prefix(package_root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            } else {
                format!(
                    "@dependencies/{}/{}",
                    package.replace("::", "/"),
                    canonical
                        .strip_prefix(package_root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                )
            },
            package: package.into(),
            offset,
            module: module.into(),
            parsed: Arc::new(parsed),
        });
        let directory = if module == package {
            canonical.parent().unwrap().to_owned()
        } else {
            canonical.with_extension("")
        };
        for decl in children {
            let name = if module.is_empty() {
                decl.name.clone()
            } else {
                format!("{module}::{}", decl.name)
            };
            let child = directory
                .join(&decl.name)
                .with_extension(canonical.extension().unwrap());
            self.load_module(&child, &name, package, depth + 1)?;
        }
        Ok(())
    }
    pub fn map_error(&self, mut error: Error) -> Error {
        let file = if let Some(path) = &error.path {
            self.files.iter().find(|f| &f.path == path)
        } else if error.line != 0 {
            self.files.iter().rev().find(|f| error.line > f.offset)
        } else {
            None
        };
        let allowed = file
            .and_then(|f| self.packages[&f.package].allowed.as_ref())
            .or(self.capabilities.as_ref());
        if let Some(serde_json::Value::Object(details)) = &mut error.details {
            details.insert("project_capabilities".into(), serde_json::json!(allowed));
            details.insert(
                "capabilities_unrestricted".into(),
                serde_json::json!(allowed.is_none()),
            );
            details.insert(
                "ffi_allowed".into(),
                serde_json::json!(allowed.is_some_and(|c| c.contains("ffi"))),
            );
            if let Some(file) = file {
                details.insert("package".into(), serde_json::json!(file.package));
                details.insert("source_key".into(), serde_json::json!(file.source_key));
                details.insert(
                    "package_identity".into(),
                    serde_json::json!(self.packages[&file.package].identity),
                );
            }
        }
        if error.path.is_none() && error.line != 0 {
            if let Some(file) = self.files.iter().rev().find(|f| error.line > f.offset) {
                error.line -= file.offset;
                error.path = Some(file.path.clone());
            }
        }
        error
    }
    pub fn compile(&self, testing: bool) -> Result<(String, usize)> {
        let (parts, tests) = self.generate(testing, true)?;
        Ok((parts.flatten(), tests))
    }
    pub fn compile_units(&self, testing: bool) -> Result<(Vec<(String, String)>, usize)> {
        let artifact = self.build_input(testing, true)?;
        Ok((artifact.units, artifact.tests))
    }
    pub fn build_input(&self, testing: bool, modules: bool) -> Result<CompiledProject> {
        let (parts, tests) = self.generate(testing, !modules)?;
        Ok(Self::generated_input(parts, tests, modules))
    }
    pub(crate) fn generated_input(
        parts: checker::Generated,
        tests: usize,
        modules: bool,
    ) -> CompiledProject {
        let c = parts.flatten();
        if !modules {
            return CompiledProject {
                c,
                units: Vec::new(),
                tests,
            };
        }
        let mut units = BTreeMap::<String, String>::new();
        for (name, code) in parts.functions {
            let module = if name.starts_with('m') && name.contains('_') {
                name.split('_').next().unwrap().to_string()
            } else {
                "m0".into()
            };
            units.entry(module).or_default().push_str(&code);
        }
        units.entry("m0".into()).or_default().push_str(&parts.entry);
        CompiledProject {
            c,
            units: units
                .into_iter()
                .map(|(name, code)| (name, format!("{}{code}", parts.preamble)))
                .collect(),
            tests,
        }
    }
    fn generate(
        &self,
        testing: bool,
        inline_functions: bool,
    ) -> Result<(checker::Generated, usize)> {
        let (checked, tests) = self.checked(testing)?;
        Ok((Self::c_parts(&checked, inline_functions), tests))
    }
    pub(crate) fn c_parts(
        checked: &crate::ir::CheckedProgram,
        inline_functions: bool,
    ) -> checker::Generated {
        let mut parts = crate::cgen::generate(checked, inline_functions);
        // C module/whole-program receipts must bind the same checked package
        // graph as native units, even when generated instructions coincide.
        let identities = checked
            .module_identities
            .iter()
            .map(|(module, identity)| {
                format!(
                    "// package-module {} {identity}\n",
                    crate::backend::sha256(module.as_bytes())
                )
            })
            .collect::<String>();
        parts.preamble.insert_str(0, &identities);
        parts
    }
    pub fn checked(&self, testing: bool) -> Result<(crate::ir::CheckedProgram, usize)> {
        self.checked_mode(testing, false)
    }
    /// Compile one checked private dispatcher. Only the enumerated tests are
    /// selectable; every source body retains the ordinary capability checks.
    pub fn checked_test_dispatcher(&self) -> Result<(crate::ir::CheckedProgram, Vec<String>)> {
        let (functions, types, _) = self.resolved_source(true, false, false, true)?;
        let checked = checker::check_owned(
            functions,
            types,
            true,
            self.ffi_allowed(),
            self.process_allowed(),
            self.net_allowed(),
        )
        .map_err(|e| self.map_error(e))?;
        let names = self
            .files
            .iter()
            .flat_map(|file| {
                file.parsed
                    .functions
                    .iter()
                    .filter(|f| f.is_test)
                    .map(|f| qualified(&file.module, &f.name))
            })
            .collect();
        let mut checked = self.finish_checked(checked, true)?;
        // The checker validates a scalar process-argument read in the private
        // runner. Substitute the runtime-only selector which consumes it and
        // hides it before entering user code. It is deliberately absent from
        // the source builtin registry and can never be requested by a user.
        let runner = checked
            .functions
            .iter_mut()
            .find(|f| f.source.name == "main")
            .unwrap();
        let mut replaced = 0;
        for op in &mut runner.body {
            if let crate::ir::Operation::Init(_, crate::ir::Rvalue::Builtin { name, args }) = op {
                if name == "args_count" && args.is_empty() {
                    *name = "internal_test_selector".into();
                    replaced += 1;
                }
            }
        }
        if replaced != 1 {
            return Err(Error::new(0, "invalid internal test dispatcher"));
        }
        Ok((checked, names))
    }
    /// Check library bodies with the normal project safety and capability rules,
    /// without requiring or synthesizing an executable entry point.
    pub fn checked_library(&self) -> Result<crate::ir::CheckedProgram> {
        self.checked_mode(false, true).map(|(program, _)| program)
    }
    /// Reload checks every source body, including tests, without requiring a
    /// process entry point. Unlike ordinary library export this is a complete
    /// checked source snapshot, never a masked or partially checked program.
    pub fn checked_reload(&self) -> Result<crate::ir::CheckedProgram> {
        let (functions, types, _) = self.resolved_source(false, true, true, false)?;
        let executable = functions.iter().any(|f| f.name == "main");
        let checked = checker::check_owned(
            functions,
            types,
            executable,
            self.ffi_allowed(),
            self.process_allowed(),
            self.net_allowed(),
        );
        self.finish_checked(checked.map_err(|e| self.map_error(e))?, false)
    }
    pub fn ffi_allowed(&self) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|c| c.contains("ffi"))
    }
    /// Launching subprocesses is opt-in, even for a legacy manifest or source.
    pub fn process_allowed(&self) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|c| c.contains("process"))
    }
    /// Network requests are opt-in, even for a legacy manifest or source.
    pub fn net_allowed(&self) -> bool {
        self.capabilities
            .as_ref()
            .is_some_and(|c| c.contains("net"))
    }
    /// Diagnostic-only hole contexts from the ordinary resolver and checker.
    /// A probe never exposes an emittable CheckedProgram.
    pub fn probe_holes(&self) -> Result<checker::HoleProbeReport> {
        let library = !self.files.iter().any(|file| {
            file.module.is_empty()
                && file
                    .parsed
                    .functions
                    .iter()
                    .any(|f| f.name == "main" && !f.is_test && f.method.is_none())
        });
        let (functions, types, _) = self.resolved_source(false, library, true, false)?;
        let mut report = checker::probe_owned(
            functions,
            types,
            !library,
            self.ffi_allowed(),
            self.process_allowed(),
            self.net_allowed(),
        )
        .map_err(|e| self.map_error(e))?;
        for diagnostic in &mut report.diagnostics {
            *diagnostic = self.map_error(diagnostic.clone());
        }
        Ok(report)
    }
    fn checked_mode(
        &self,
        testing: bool,
        library: bool,
    ) -> Result<(crate::ir::CheckedProgram, usize)> {
        let (functions, types, tests) = self.resolved_source(testing, library, false, false)?;
        let checked = checker::check_owned(
            functions,
            types,
            !library,
            self.ffi_allowed(),
            self.process_allowed(),
            self.net_allowed(),
        );
        Ok((
            self.finish_checked(checked.map_err(|e| self.map_error(e))?, testing)?,
            tests,
        ))
    }
    fn finish_checked(
        &self,
        mut checked: crate::ir::CheckedProgram,
        synthetic_runner: bool,
    ) -> Result<crate::ir::CheckedProgram> {
        use crate::ir::{Operation, Rvalue};
        let mut required = BTreeMap::<String, BTreeSet<String>>::new();
        let mut calls = BTreeMap::<String, BTreeSet<String>>::new();
        for f in &checked.functions {
            let caps = required.entry(f.source.name.clone()).or_default();
            if f.source.external_symbol.is_some() {
                caps.insert("ffi".into());
            }
            let edges = calls.entry(f.source.name.clone()).or_default();
            let mut pending: Vec<_> = f.body.iter().collect();
            while let Some(op) = pending.pop() {
                match op {
                    Operation::Init(_, Rvalue::Builtin { name, .. }) => {
                        if let Some(cap) = builtin_capability(name) {
                            caps.insert(cap.into());
                        }
                    }
                    Operation::Init(_, Rvalue::Call { name, spawn, .. }) => {
                        edges.insert(name.clone());
                        if *spawn {
                            caps.insert("threads".into());
                        }
                    }
                    Operation::If { yes, no, .. } => {
                        pending.extend(yes);
                        pending.extend(no);
                    }
                    Operation::Loop {
                        condition_code,
                        body,
                        ..
                    } => {
                        pending.extend(condition_code);
                        pending.extend(body);
                    }
                    Operation::Switch { arms, .. } => {
                        for arm in arms {
                            pending.extend(arm);
                        }
                    }
                    _ => {}
                }
            }
        }
        loop {
            let previous = required.clone();
            for (name, edges) in &calls {
                for target in edges {
                    if let Some(caps) = previous.get(target) {
                        required.get_mut(name).unwrap().extend(caps.iter().cloned());
                    }
                }
            }
            if previous == required {
                break;
            }
        }
        for f in &checked.functions {
            // Compiler-generated dispatch reads one selector. It never changes
            // the authority applied to any user function, including user main.
            if synthetic_runner && f.source.name == "main" {
                continue;
            }
            if let Some(file) = self
                .files
                .iter()
                .find(|file| file.module == f.source.module)
            {
                let allowed = &self.packages[&file.package].allowed;
                for capability in &required[&f.source.name] {
                    let granted = allowed.as_ref().map_or(
                        !matches!(capability.as_str(), "ffi" | "process" | "net"),
                        |c| c.contains(capability),
                    );
                    if !granted {
                        let package = if file.package.is_empty() {
                            &self.name
                        } else {
                            &file.package
                        };
                        return Err(self.map_error(Error::new(f.source.line, format!("package {package} does not grant capability {capability} required transitively by {}", f.source.name))));
                    }
                }
            }
        }
        for file in &self.files {
            checked.module_identities.insert(
                file.module.clone(),
                crate::backend::sha256(
                    format!(
                        "{}\n{}\n{}",
                        self.packages[&file.package].identity,
                        file.source_key,
                        file.path.display()
                    )
                    .as_bytes(),
                ),
            );
        }
        Ok(checked)
    }
    fn resolved_source(
        &self,
        testing: bool,
        library: bool,
        include_tests: bool,
        dispatcher: bool,
    ) -> Result<(Vec<Function>, crate::types::TypeTable, usize)> {
        let mut type_registry = BTreeMap::<String, (usize, String, bool)>::new();
        for name in ["ReadResult", "WriteResult", "ProcessOutput", "ProcessRun"] {
            type_registry.insert(name.into(), (type_registry.len(), String::new(), true));
        }
        for file in &self.files {
            for d in &file.parsed.types {
                if standard_result(&d.name).is_some() {
                    return Err(
                        Error::new(d.line, "reserved standard result type").in_file(&file.path)
                    );
                }
                let key = qualified(&file.module, &d.name);
                let id = type_registry.len();
                if type_registry
                    .insert(key, (id, file.module.clone(), d.public))
                    .is_some()
                {
                    return Err(Error::new(d.line, "duplicate data type").in_file(&file.path));
                }
            }
        }
        // Type-id stability: the 1.2 networking prelude takes the last two
        // ids, after every user and package type, so no existing program's
        // type ids (and C type names `zn_type_<id>`) or layouts change.
        let net_response = type_registry.len();
        for name in ["NetResponse", "NetResult"] {
            type_registry.insert(name.into(), (type_registry.len(), String::new(), true));
        }
        let resolve_type =
            |name: &str, file: &SourceFile, line: usize| -> Result<(usize, String)> {
                let key = if let Some(standard) = standard_result(name) {
                    standard.into()
                } else {
                    self.qualified_for(file, name)
                };
                let (id, owner, public) = type_registry
                    .get(&key)
                    .ok_or_else(|| Error::new(line, format!("unknown data type {name}")))?;
                if owner != &file.module && !*public {
                    return Err(Error::new(line, format!("data type {name} is private")));
                }
                Ok((*id, key))
            };
        let mut definitions = vec![None; type_registry.len()];
        for (id, name) in ["ReadResult", "WriteResult"].iter().enumerate() {
            definitions[id] = Some(DataDecl {
                name: (*name).into(),
                public: true,
                line: 0,
                kind: DataKind::Enum(vec![
                    FieldDecl {
                        name: "Ok".into(),
                        ty: if id == 0 { Type::Text } else { Type::Unit },
                    },
                    FieldDecl {
                        name: "Err".into(),
                        ty: Type::Text,
                    },
                ]),
            });
        }
        definitions[2] = Some(DataDecl {
            name: "ProcessOutput".into(),
            public: true,
            line: 0,
            kind: DataKind::Record(vec![
                FieldDecl {
                    name: "status".into(),
                    ty: Type::Int,
                },
                FieldDecl {
                    name: "stdout".into(),
                    ty: Type::Text,
                },
                FieldDecl {
                    name: "stderr".into(),
                    ty: Type::Text,
                },
            ]),
        });
        definitions[3] = Some(DataDecl {
            name: "ProcessRun".into(),
            public: true,
            line: 0,
            kind: DataKind::Enum(vec![
                FieldDecl {
                    name: "Output".into(),
                    ty: Type::Named(2),
                },
                FieldDecl {
                    name: "Error".into(),
                    ty: Type::Text,
                },
            ]),
        });
        definitions[net_response] = Some(DataDecl {
            name: "NetResponse".into(),
            public: true,
            line: 0,
            kind: DataKind::Record(vec![
                FieldDecl {
                    name: "status".into(),
                    ty: Type::Int,
                },
                FieldDecl {
                    name: "body".into(),
                    ty: Type::Text,
                },
            ]),
        });
        definitions[net_response + 1] = Some(DataDecl {
            name: "NetResult".into(),
            public: true,
            line: 0,
            kind: DataKind::Enum(vec![
                FieldDecl {
                    name: "Response".into(),
                    ty: Type::Named(net_response),
                },
                FieldDecl {
                    name: "Error".into(),
                    ty: Type::Text,
                },
            ]),
        });
        let mut mappings = Vec::with_capacity(self.files.len());
        for file in &self.files {
            let map = file
                .parsed
                .type_names
                .iter()
                .map(|name| {
                    resolve_type(name, file, 1 + file.offset).map(|(id, _)| Type::Named(id))
                })
                .collect::<Result<Vec<_>>>()
                .map_err(|e| self.map_error(e))?;
            for original in &file.parsed.types {
                let mut d = original.clone();
                let (id, key) = resolve_type(&d.name, file, d.line + file.offset)
                    .map_err(|e| self.map_error(e))?;
                d.name = key;
                d.line += file.offset;
                let fields = match &mut d.kind {
                    DataKind::Record(f)
                    | DataKind::Enum(f)
                    | DataKind::Vector(f)
                    | DataKind::SoaVector(f)
                    | DataKind::Arena(f)
                    | DataKind::Channel(f)
                    | DataKind::Map(f) => f,
                };
                for f in fields {
                    f.ty = remap_type(f.ty, &map);
                }
                definitions[id] = Some(d);
            }
            mappings.push(map);
        }
        let types =
            crate::types::TypeTable::new(definitions.into_iter().map(Option::unwrap).collect())
                .map_err(|e| self.map_error(e))?;
        for (file_id, file) in self.files.iter().enumerate() {
            for implementation in &file.parsed.implementations {
                let Type::Named(id) = remap_type(implementation.owner, &mappings[file_id]) else {
                    unreachable!()
                };
                let name = &types.definitions[id].name;
                if ["ReadResult", "WriteResult", "NetResponse", "NetResult"]
                    .contains(&name.as_str())
                    || type_registry[name].1 != file.module
                {
                    return Err(Error::new(
                        implementation.line,
                        "impl requires a type declared in this module",
                    )
                    .in_file(&file.path));
                }
            }
        }
        let mut registry = BTreeMap::new();
        let mut method_targets = BTreeSet::new();
        for (module_id, file) in self.files.iter().enumerate() {
            for (function_id, f) in file.parsed.functions.iter().enumerate() {
                if checker::reserved_builtin(&f.name) {
                    return Err(
                        Error::new(f.line, "reserved builtin function name").in_file(&file.path)
                    );
                }
                let key = if let Some(method) = &f.method {
                    let Type::Named(id) = remap_type(method.owner, &mappings[module_id]) else {
                        unreachable!()
                    };
                    format!("{}::{}", types.definitions[id].name, method.name)
                } else if file.module.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}::{}", file.module, f.name)
                };
                let symbol = function_symbol(module_id, function_id, &file.module, f);
                if f.method.is_some() {
                    method_targets.insert(key.clone());
                }
                if registry
                    .insert(
                        key,
                        (
                            symbol,
                            file.module.clone(),
                            f.public,
                            f.is_test || (f.name == "main" && file.module == file.package),
                        ),
                    )
                    .is_some()
                {
                    return Err(Error::new(f.line, "duplicate function name").in_file(&file.path));
                }
            }
        }
        let mut functions = Vec::with_capacity(
            self.files
                .iter()
                .map(|file| file.parsed.functions.len())
                .sum(),
        );
        let mut tests = Vec::new();
        for (file_id, file) in self.files.iter().enumerate() {
            for original in &file.parsed.functions {
                if library && original.is_test && !include_tests {
                    continue;
                }
                if original.external_symbol.is_some()
                    && !self.packages[&file.package]
                        .allowed
                        .as_ref()
                        .is_some_and(|c| c.contains("ffi"))
                {
                    return Err(Error::new(
                        original.line,
                        "project does not grant capability ffi for external declaration",
                    )
                    .in_file(&file.path));
                }
                let mut f = original.clone();
                f.module = file.module.clone();
                if let Some(method) = &mut f.method {
                    method.owner = remap_type(method.owner, &mappings[file_id]);
                    let Type::Named(id) = method.owner else {
                        unreachable!()
                    };
                    if types
                        .variant(&format!("{}::{}", types.definitions[id].name, method.name))
                        .is_some()
                    {
                        return Err(Error::new(
                            f.line,
                            "method name conflicts with an enum variant",
                        )
                        .in_file(&file.path));
                    }
                }
                for p in &mut f.params {
                    p.ty = remap_type(p.ty, &mappings[file_id]);
                }
                f.ret = remap_type(f.ret, &mappings[file_id]);
                if f.public {
                    for ty in f.params.iter().map(|p| p.ty).chain(std::iter::once(f.ret)) {
                        if let Type::Named(id) | Type::Id(id) | Type::VectorView(id) = ty {
                            if !types.definitions[id].public {
                                return Err(Error::new(
                                    f.line,
                                    "public function signatures require public data types",
                                )
                                .in_file(&file.path));
                            }
                        }
                    }
                }
                let key = if let Some(method) = &f.method {
                    let Type::Named(id) = method.owner else {
                        unreachable!()
                    };
                    format!("{}::{}", types.definitions[id].name, method.name)
                } else if file.module.is_empty() {
                    f.name.clone()
                } else {
                    format!("{}::{}", file.module, f.name)
                };
                f.name = registry[&key].0.clone();
                f.line += file.offset;
                f.source_offset = file.offset;
                let mut resolve = |e: &mut Expr| -> Result<()> {
                    e.line += file.offset;
                    if let ExprKind::Hole(ty, _) = &mut e.kind {
                        *ty = remap_type(*ty, &mappings[file_id]);
                    }
                    if let ExprKind::Call(name, _) = &e.kind {
                        if library && matches!(name.as_str(), "arg" | "args_count" | "exit_code") {
                            return Err(Error::new(
                                e.line,
                                format!("executable-only builtin {name} is unavailable in a static library; no process entry context is initialized"),
                            ));
                        }
                    }
                    let effect = match &e.kind {
                        ExprKind::Spawn(_, _) => Some("threads"),
                        ExprKind::Call(name, _) => builtin_capability(name),
                        _ => None,
                    };
                    if let Some(effect) = effect {
                        let allowed = &self.packages[&file.package].allowed;
                        if allowed
                            .as_ref()
                            .map_or(effect == "process", |caps| !caps.contains(effect))
                        {
                            return Err(Error::new(
                                e.line,
                                format!("project does not grant capability {effect}"),
                            ));
                        }
                    }
                    let name = match &mut e.kind {
                        ExprKind::Record(n, _) => {
                            *n = resolve_type(n, file, e.line)?.1;
                            return Ok(());
                        }
                        ExprKind::Call(n, _) | ExprKind::Spawn(n, _) => n,
                        _ => return Ok(()),
                    };
                    let target = self.qualified_for(file, name);
                    if checker::BUILTINS.contains(&name.as_str()) && !registry.contains_key(&target)
                    {
                        return Ok(());
                    }
                    if type_registry.contains_key(&target) {
                        let (id, _) = resolve_type(name, file, e.line)?;
                        if types.vector_element(Type::Named(id)).is_some()
                            || types.arena_element(Type::Named(id)).is_some()
                            || types.channel_element(Type::Named(id)).is_some()
                            || types.map_types(Type::Named(id)).is_some()
                        {
                            *name = target;
                            return Ok(());
                        }
                    }
                    if let Some((type_path, variant)) = name
                        .rsplit_once("::")
                        .filter(|_| !method_targets.contains(&target))
                    {
                        if let Some(standard) = standard_result(type_path) {
                            *name = format!("{standard}::{variant}");
                            return Ok(());
                        }
                        let type_key = self.qualified_for(file, type_path);
                        if type_registry.contains_key(&type_key) {
                            resolve_type(type_path, file, e.line)?;
                            *name = target;
                            return Ok(());
                        }
                    }
                    let (symbol, owner, public, test) = registry
                        .get(&target)
                        .ok_or_else(|| Error::new(e.line, format!("unknown function {name}")))?;
                    if owner != &file.module && !public {
                        return Err(Error::new(
                            e.line,
                            format!("function {target} is private; declare pub fn to export it"),
                        ));
                    }
                    if *test || symbol == "main" {
                        return Err(Error::new(
                            e.line,
                            "entry points and tests cannot be called or spawned",
                        ));
                    }
                    *name = symbol.clone();
                    Ok(())
                };
                for e in f.requires.iter_mut().chain(f.ensures.iter_mut()) {
                    walk_expr(e, &mut resolve).map_err(|e| self.map_error(e))?;
                }
                walk_body(&mut f.body, file.offset, &mut resolve).map_err(|e| self.map_error(e))?;
                if f.is_test {
                    tests.push((key, f.name.clone()));
                }
                if testing && f.name == "main" {
                    f.name = "zenith_user_entry".into();
                }
                functions.push(f);
            }
        }
        if testing {
            if tests.is_empty() {
                return Err(Error::new(0, "no tests found; declare test name() { ... }"));
            }
            let mut runner = String::from("fn main() {\n");
            if dispatcher {
                runner.push_str("let anode_test_selector = args_count();\n");
            }
            for (index, (name, symbol)) in tests.iter().enumerate() {
                if dispatcher {
                    runner.push_str(&format!(
                        "if anode_test_selector == {index} {{ {symbol}(); return; }}\n"
                    ));
                } else {
                    runner.push_str(&format!("print \"test {name}\"; {symbol}();\n"));
                }
            }
            if dispatcher {
                runner.push_str("assert false; }");
            } else {
                runner.push_str(&format!("print \"{} tests passed\"; }}", tests.len()));
            }
            functions.extend(parse(&runner)?);
        }
        Ok((functions, types, tests.len()))
    }
    pub fn bundle(&self) -> String {
        self.files
            .iter()
            .map(|f| format!("// FILE: {}\n{}", f.source_key, f.source))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    pub fn output(&self, testing: bool, optimize: bool, sanitize: bool) -> PathBuf {
        let suffix = format!(
            "{}{}{}",
            if testing { "-tests" } else { "" },
            if optimize { "-release" } else { "" },
            if sanitize { "-asan" } else { "" }
        );
        if self.managed {
            self.root
                .join(".zenith/build")
                .join(format!("{}{suffix}.exe", self.name))
        } else {
            PathBuf::from("out").join(format!("{}{suffix}.exe", self.name))
        }
    }
}

fn walk_expr(e: &mut Expr, visit: &mut impl FnMut(&mut Expr) -> Result<()>) -> Result<()> {
    visit(e)?;
    match &mut e.kind {
        ExprKind::MethodCall(receiver, _, args) => {
            walk_expr(receiver, visit)?;
            for arg in args {
                walk_expr(arg, visit)?;
            }
        }
        ExprKind::Unary(_, e) | ExprKind::Try(e) => walk_expr(e, visit)?,
        ExprKind::Field(e, _) | ExprKind::Repeat(e, _) => walk_expr(e, visit)?,
        ExprKind::Record(_, fields) => {
            for (_, e) in fields {
                walk_expr(e, visit)?;
            }
        }
        ExprKind::Binary(_, a, b) => {
            walk_expr(a, visit)?;
            walk_expr(b, visit)?;
        }
        ExprKind::Array(args) | ExprKind::Call(_, args) | ExprKind::Spawn(_, args) => {
            for arg in args {
                walk_expr(arg, visit)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn walk_body(
    body: &mut [Stmt],
    offset: usize,
    visit: &mut impl FnMut(&mut Expr) -> Result<()>,
) -> Result<()> {
    // Preserve depth-first source order without a Rust call frame per nested
    // statement. In particular, a valid else-if chain must not exhaust the
    // Windows compiler process's main stack while resolving its source names.
    enum Work<'a> {
        Statement(&'a mut Stmt),
        Arm(&'a mut MatchArm),
    }
    let mut pending: Vec<_> = body.iter_mut().rev().map(Work::Statement).collect();
    while let Some(work) = pending.pop() {
        let s = match work {
            Work::Statement(s) => s,
            Work::Arm(arm) => {
                if arm.path != "_" {
                    let mut path = Expr {
                        line: arm.line,
                        kind: ExprKind::Call(arm.path.clone(), Vec::new()),
                    };
                    visit(&mut path)?;
                    if let ExprKind::Call(name, _) = path.kind {
                        arm.path = name;
                    }
                }
                arm.line += offset;
                pending.extend(arm.body.iter_mut().rev().map(Work::Statement));
                continue;
            }
        };
        s.line += offset;
        match &mut s.kind {
            StmtKind::ForRange {
                start, end, body, ..
            } => {
                walk_expr(start, visit)?;
                walk_expr(end, visit)?;
                pending.extend(body.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::ForEach { owner, body, .. } => {
                walk_expr(owner, visit)?;
                pending.extend(body.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::Element {
                owner, index, body, ..
            } => {
                walk_expr(owner, visit)?;
                walk_expr(index, visit)?;
                pending.extend(body.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::ElementPair {
                owner,
                indices,
                body,
                ..
            } => {
                walk_expr(owner, visit)?;
                for index in indices {
                    walk_expr(index, visit)?;
                }
                pending.extend(body.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::Region {
                owner,
                start,
                end,
                body,
                ..
            } => {
                walk_expr(owner, visit)?;
                walk_expr(start, visit)?;
                walk_expr(end, visit)?;
                pending.extend(body.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::Let(_, _, e)
            | StmtKind::Assign(_, e)
            | StmtKind::Expr(e)
            | StmtKind::Print(e)
            | StmtKind::Assert(e) => walk_expr(e, visit)?,
            StmtKind::Return(e) => {
                if let Some(e) = e {
                    walk_expr(e, visit)?;
                }
            }
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::If(e, a, b) => {
                walk_expr(e, visit)?;
                pending.extend(b.iter_mut().rev().map(Work::Statement));
                pending.extend(a.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::While(e, b) => {
                walk_expr(e, visit)?;
                pending.extend(b.iter_mut().rev().map(Work::Statement));
            }
            StmtKind::AssignField(a, b) => {
                walk_expr(a, visit)?;
                walk_expr(b, visit)?;
            }
            StmtKind::Match(e, arms) => {
                walk_expr(e, visit)?;
                pending.extend(arms.iter_mut().rev().map(Work::Arm));
            }
        }
    }
    Ok(())
}

fn qualified(module: &str, name: &str) -> String {
    if let Some(root) = name.strip_prefix("crate::") {
        root.into()
    } else if module.is_empty() {
        name.into()
    } else {
        format!("{module}::{name}")
    }
}
fn builtin_capability(name: &str) -> Option<&'static str> {
    match name {
        "read_text" | "try_read_text" | "file_exists" | "list_dir" | "try_read_bytes" => {
            Some("file_read")
        }
        "write_text" | "try_write_text" | "try_write_bytes" | "create_dir" | "remove_file" => {
            Some("file_write")
        }
        "arg" | "args_count" | "env_var" => Some("process_args"),
        "process_run" => Some("process"),
        "net_request" | "net_serve_once" | "http_request" => Some("net"),
        _ => None,
    }
}
fn standard_result(name: &str) -> Option<&'static str> {
    match name {
        "ReadResult" | "FileRead" => Some("ReadResult"),
        "WriteResult" => Some("WriteResult"),
        "ProcessOutput" => Some("ProcessOutput"),
        "ProcessRun" => Some("ProcessRun"),
        "NetResponse" => Some("NetResponse"),
        "NetResult" => Some("NetResult"),
        _ => None,
    }
}
/// One symbol mapping for compilation and compiler-backed agent context.
pub(crate) fn function_symbol(
    module_id: usize,
    function_id: usize,
    module: &str,
    function: &Function,
) -> String {
    if module.is_empty()
        && function.name == "main"
        && !function.is_test
        && function.method.is_none()
    {
        "main".into()
    } else if function.method.is_some() {
        // No source identifier can start with a digit after mN_.
        format!("m{module_id}_{function_id}_method")
    } else {
        format!("m{module_id}_{}", function.name)
    }
}
fn remap_type(ty: Type, map: &[Type]) -> Type {
    match ty {
        Type::TaskOf(TaskResult::Channel(id)) => {
            let Type::Named(mapped) = map[id] else {
                unreachable!()
            };
            Type::TaskOf(TaskResult::Channel(mapped))
        }
        Type::Named(id) => map[id],
        Type::Id(id) => {
            let Type::Named(mapped) = map[id] else {
                unreachable!()
            };
            Type::Id(mapped)
        }
        Type::VectorView(id) => {
            let Type::Named(mapped) = map[id] else {
                unreachable!()
            };
            Type::VectorView(mapped)
        }
        _ => {
            if let Some(result) = ty.task_result() {
                Type::task_of(remap_type(result, map)).unwrap()
            } else {
                ty
            }
        }
    }
}

/// Agent guides written by `anode new`, so a coding agent opening a fresh
/// project finds the language rules, CLI and standard library packages.
const SCAFFOLD_AGENTS_MD: &str = include_str!("scaffold_agents.md");
const SCAFFOLD_CLAUDE_MD: &str = include_str!("scaffold_claude.md");

pub fn create(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| identifier(n))
        .ok_or_else(|| {
            Error::new(
                0,
                "choose a project folder name using letters, digits, - or _",
            )
        })?;
    // create_dir deliberately refuses an existing directory, preventing overwrites.
    fs::create_dir(path).map_err(|e| io(path, e))?;
    fs::create_dir(path.join("src")).map_err(|e| io(path, e))?;
    fs::write(path.join("Anode.toml"), format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[build]\nentry = \"src/main.an\"\ncache_mb = 0\n\n[capabilities]\nfile_read = false\nfile_write = false\nprocess_args = false\nprocess = false\nthreads = false\nffi = false\nnet = false\n")).map_err(|e| io(path,e))?;
    fs::write(path.join("src/main.an"), "mod greeting;\n\nfn main() {\n    let message = greeting::message();\n    print message;\n}\n").map_err(|e| io(path,e))?;
    fs::write(path.join("src/greeting.an"), "pub fn message() -> text {\n    return \"Hello from Anode\";\n}\n\ntest message_is_not_empty() {\n    let text = message();\n    assert len(text) > 0;\n}\n").map_err(|e| io(path,e))?;
    fs::write(path.join(".gitignore"), "/.zenith/\n").map_err(|e| io(path, e))?;
    fs::write(path.join("AGENTS.md"), SCAFFOLD_AGENTS_MD).map_err(|e| io(path, e))?;
    fs::write(path.join("CLAUDE.md"), SCAFFOLD_CLAUDE_MD).map_err(|e| io(path, e))?;
    Ok(())
}

#[cfg(test)]
mod net_capability_tests {
    use super::*;

    /// root -> a -> b, every package granting net; b::fetch calls
    /// net_request and a::relay calls it.
    fn chain(root: &Path) -> Project {
        let package = |dir: &Path, name: &str, entry: &str, source: &str, deps: &str| {
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(
                dir.join("Anode.toml"),
                format!("[package]\nname=\"{name}\"\n[build]\nentry=\"src/{entry}\"\n[capabilities]\nnet = true\n{deps}"),
            )
            .unwrap();
            fs::write(dir.join("src").join(entry), source).unwrap();
        };
        let a = root.join("a");
        let b = a.join("b");
        package(
            &b,
            "b",
            "lib.an",
            "pub fn fetch()->NetResult{return net_request(\"loopback://echo\",\"x\");}",
            "",
        );
        package(
            &a,
            "a",
            "lib.an",
            "pub fn relay()->NetResult{return b::fetch();}",
            "[dependencies]\nb=\"b\"\n",
        );
        package(
            root,
            "root",
            "main.an",
            "fn main(){let r=a::relay();}",
            "[dependencies]\na=\"a\"\n",
        );
        Project::load(root).unwrap()
    }

    /// Manifest loading intersects every dependency grant with its parent's,
    /// so the transitive gate cannot be reached through manifests alone; it
    /// is defense in depth. Revoke a grant after loading and check that the
    /// gate names net with the existing 1.1 message format.
    #[test]
    fn net_transitive_denial_keeps_the_existing_message_format() {
        let root = std::env::temp_dir().join(format!("zn-v120a-net-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut project = chain(&root);
        project.checked(false).unwrap();
        project.packages.get_mut("a").unwrap().allowed = Some(BTreeSet::new());
        let message = project.checked(false).unwrap_err().message;
        assert!(
            message
                .starts_with("package a does not grant capability net required transitively by m")
                && message.ends_with("_relay"),
            "{message}"
        );
        // An unrestricted (legacy) root never implicitly holds net.
        let mut project = chain(&root);
        project.packages.get_mut("").unwrap().allowed = None;
        assert_eq!(
            project.checked(false).unwrap_err().message,
            "package root does not grant capability net required transitively by main"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
