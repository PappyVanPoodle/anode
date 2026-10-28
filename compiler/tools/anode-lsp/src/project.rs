use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

/// Normalize a filesystem path:
/// - Canonicalizes when the path exists on disk (or canonicalizes the existing parent prefix).
/// - Strips the Windows `\\?\` verbatim prefix produced by `std::fs::canonicalize`.
/// - Normalizes Windows drive letters to uppercase (`C:\...`) for deterministic key equality.
pub fn normalize_path(path: &Path) -> PathBuf {
    let resolved = if let Ok(canon) = path.canonicalize() {
        canon
    } else {
        // Canonicalize the longest existing ancestor, then append remaining components.
        let mut suffix = Vec::new();
        let mut cursor = path;
        let base = loop {
            if let Ok(canon) = cursor.canonicalize() {
                break Some(canon);
            }
            if let Some(name) = cursor.file_name() {
                suffix.push(name.to_os_string());
            }
            if let Some(parent) = cursor.parent() {
                if parent.as_os_str().is_empty() || parent == cursor {
                    break None;
                }
                cursor = parent;
            } else {
                break None;
            }
        };
        let mut out = base.unwrap_or_else(|| {
            if path.is_absolute() {
                PathBuf::new()
            } else {
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
            }
        });
        if out.as_os_str().is_empty() {
            out = path.to_path_buf();
        } else {
            for part in suffix.into_iter().rev() {
                out.push(part);
            }
        }
        clean_components(&out)
    };

    strip_verbatim_and_normalize_drive(&resolved)
}

fn clean_components(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn strip_verbatim_and_normalize_drive(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let stripped = if let Some(rest) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = raw.strip_prefix(r"\\?\") {
        rest.to_owned()
    } else if let Some(rest) = raw.strip_prefix("//?/") {
        rest.to_owned()
    } else {
        raw.into_owned()
    };

    let bytes = stripped.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let mut chars = stripped.chars();
        let drive = chars.next().unwrap().to_ascii_uppercase();
        let rest: String = chars.collect();
        PathBuf::from(format!("{drive}{rest}"))
    } else {
        PathBuf::from(stripped)
    }
}

/// Convert a `file://` URI into a normalized `PathBuf`.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let decoded = percent_decode(rest);

    // Handle `file:///C:/...` or `file:///c:/...`
    let path_str = if decoded.starts_with('/')
        && decoded
            .as_bytes()
            .get(1)
            .is_some_and(|b| b.is_ascii_alphabetic())
        && decoded.as_bytes().get(2) == Some(&b':')
    {
        &decoded[1..]
    } else {
        &decoded
    };

    #[cfg(windows)]
    let platform_str = path_str.replace('/', r"\");
    #[cfg(not(windows))]
    let platform_str = path_str.to_owned();

    Some(normalize_path(Path::new(&platform_str)))
}

/// Convert a filesystem path into a canonical `file://` URI.
pub fn path_to_uri(path: &Path) -> String {
    let norm = normalize_path(path);
    let slash = norm.to_string_lossy().replace('\\', "/");
    let encoded = percent_encode_path(&slash);
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_path(slash_path: &str) -> String {
    let mut out = String::with_capacity(slash_path.len());
    for (idx, b) in slash_path.bytes().enumerate() {
        // Keep drive colon `C:` unencoded at index 1
        let safe = b.is_ascii_alphanumeric()
            || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~')
            || (idx == 1 && b == b':');
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectLayout {
    /// Canonical project root directory (`project.root` inside `anode.exe`).
    pub root: PathBuf,
    /// Target path argument passed to `anode.exe agent <agent_target>`.
    pub agent_target: PathBuf,
    /// Canonical path of the root module file (`src/main.an`, `src/main.zn`, or standalone file).
    pub entry_file: PathBuf,
    /// True if governed by `Anode.toml` or `Zenith.toml`.
    pub managed: bool,
}

impl ProjectLayout {
    /// Discover the Anode project layout for a given `.an`, `.zn`, or manifest file.
    pub fn discover(file_path: &Path) -> Self {
        let norm_file = normalize_path(file_path);

        // If `norm_file` itself is Anode.toml or Zenith.toml:
        if let Some(name) = norm_file.file_name().and_then(|n| n.to_str()) {
            if name == "Anode.toml" || name == "Zenith.toml" {
                let root = norm_file
                    .parent()
                    .map(normalize_path)
                    .unwrap_or_else(|| PathBuf::from("."));
                let entry_rel = parse_manifest_entry(&norm_file);
                let entry_file = normalize_path(&root.join(entry_rel));
                return Self {
                    root,
                    agent_target: norm_file,
                    entry_file,
                    managed: true,
                };
            }
        }

        // Walk ancestor directories looking for Anode.toml or Zenith.toml.
        let start_dir = if norm_file.is_dir() {
            Some(norm_file.as_path())
        } else {
            norm_file.parent()
        };

        for dir in start_dir.into_iter().flat_map(Path::ancestors) {
            let anode_toml = dir.join("Anode.toml");
            let zenith_toml = dir.join("Zenith.toml");
            let has_anode = anode_toml.is_file();
            let has_zenith = zenith_toml.is_file();
            if has_anode || has_zenith {
                let root = normalize_path(dir);
                let manifest_path = if has_anode && has_zenith {
                    if norm_file.extension().is_some_and(|e| e == "zn") {
                        root.join("Zenith.toml")
                    } else {
                        root.join("Anode.toml")
                    }
                } else if has_anode {
                    root.join("Anode.toml")
                } else {
                    root.join("Zenith.toml")
                };
                let agent_target = if has_anode && has_zenith {
                    manifest_path.clone()
                } else {
                    root.clone()
                };
                let entry_rel = parse_manifest_entry(&manifest_path);
                let entry_file = normalize_path(&root.join(entry_rel));
                return Self {
                    root,
                    agent_target,
                    entry_file,
                    managed: true,
                };
            }
        }

        // Standalone `.an` / `.zn` file with no manifest.
        let root = norm_file
            .parent()
            .map(normalize_path)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root,
            agent_target: norm_file.clone(),
            entry_file: norm_file,
            managed: false,
        }
    }

    /// Compute the project-relative forward-slash path used in `overlays` by `anode.exe agent`.
    pub fn overlay_key(&self, file_path: &Path) -> Option<String> {
        self.overlay_keys(file_path).into_iter().next()
    }

    /// All package instantiations of a physical source share its live overlay.
    pub fn overlay_keys(&self, file_path: &Path) -> Vec<String> {
        let norm = normalize_path(file_path);
        let memberships = self
            .dependency_roots()
            .into_iter()
            .filter_map(|(prefix, root, _)| {
                norm.strip_prefix(&root).ok().map(|rel| {
                    (
                        root.components().count(),
                        format!(
                            "@dependencies/{prefix}/{}",
                            rel.to_string_lossy().replace('\\', "/")
                        ),
                    )
                })
            })
            .collect::<Vec<_>>();
        let depth = memberships
            .iter()
            .map(|(depth, _)| *depth)
            .max()
            .unwrap_or(0);
        let mut keys = memberships
            .into_iter()
            .filter(|(d, _)| *d == depth)
            .map(|(_, key)| key)
            .collect::<Vec<_>>();
        if keys.is_empty() {
            if let Ok(relative) = norm.strip_prefix(&self.root) {
                keys.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn dependency_roots(&self) -> Vec<(String, PathBuf, PathBuf)> {
        fn visit(
            root: &Path,
            prefix: &str,
            ancestors: &BTreeSet<PathBuf>,
            out: &mut Vec<(String, PathBuf, PathBuf)>,
        ) {
            if ancestors.len() > 32 || out.len() >= 128 {
                return;
            }
            let root = normalize_path(root);
            if ancestors.contains(&root) {
                return;
            }
            let mut ancestors = ancestors.clone();
            ancestors.insert(root.clone());
            let manifest = package_manifest(&root);
            let Ok(text) = fs::read_to_string(&manifest) else {
                return;
            };
            let mut section = "";
            for raw in text.lines() {
                let line = manifest_code(raw);
                if line.starts_with('[') {
                    section = line;
                    continue;
                }
                if section != "[dependencies]" {
                    continue;
                }
                let Some((alias, value)) = line.split_once('=') else {
                    continue;
                };
                let alias = alias.trim();
                if alias.is_empty()
                    || !alias
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    continue;
                }
                let value = value.trim();
                let path = if let Some(table) =
                    value.strip_prefix('{').and_then(|v| v.strip_suffix('}'))
                {
                    table.split(',').find_map(|part| {
                        part.split_once('=')
                            .filter(|(key, _)| key.trim() == "path")
                            .map(|(_, v)| v.trim())
                    })
                } else {
                    Some(value)
                };
                let Some(path) = path
                    .and_then(|v| v.strip_prefix('"'))
                    .and_then(|v| v.strip_suffix('"'))
                else {
                    continue;
                };
                let dependency = normalize_path(&root.join(path));
                let manifest = package_manifest(&dependency);
                if !manifest.is_file() || ancestors.contains(&dependency) {
                    continue;
                }
                let qualified = if prefix.is_empty() {
                    alias.into()
                } else {
                    format!("{prefix}/{alias}")
                };
                out.push((
                    qualified.clone(),
                    dependency.clone(),
                    dependency.join(parse_manifest_entry(&manifest)),
                ));
                visit(&dependency, &qualified, &ancestors, out);
            }
        }
        let mut roots = Vec::new();
        if self.managed {
            visit(&self.root, "", &BTreeSet::new(), &mut roots);
        }
        roots.sort_by(|a, b| a.0.cmp(&b.0));
        roots
    }

    pub fn snapshot_files(&self) -> BTreeSet<PathBuf> {
        let mut files = self.discover_reachable_disk_files();
        if self.managed {
            files.insert(package_manifest(&self.root));
            for (_, root, _) in self.dependency_roots() {
                files.insert(package_manifest(&root));
            }
        }
        files
    }

    /// Compute the compiler module qualifier for `file_path` (e.g. `""` for root, `"search"` for `src/search.an`).
    pub fn module_path_for_file(&self, file_path: &Path) -> String {
        let norm = normalize_path(file_path);
        let mut dependencies = self.dependency_roots();
        dependencies.sort_by(|a, b| {
            b.1.components()
                .count()
                .cmp(&a.1.components().count())
                .then_with(|| a.0.cmp(&b.0))
        });
        for (prefix, root, entry) in dependencies {
            if norm.starts_with(&root) {
                let prefix = prefix.replace('/', "::");
                if norm == normalize_path(&entry) {
                    return prefix;
                }
                if let Ok(rel) = norm.strip_prefix(entry.parent().unwrap_or(&root)) {
                    let tail = rel
                        .with_extension("")
                        .to_string_lossy()
                        .replace(['\\', '/'], "::");
                    return format!("{prefix}::{tail}");
                }
            }
        }
        if norm == self.entry_file {
            return String::new();
        }
        let entry_dir = self.entry_file.parent().unwrap_or(&self.root);
        let Ok(rel) = norm.strip_prefix(entry_dir) else {
            return String::new();
        };
        let without_ext = rel.with_extension("");
        let mut parts = Vec::new();
        for comp in without_ext.components() {
            if let Component::Normal(s) = comp {
                parts.push(s.to_string_lossy().into_owned());
            }
        }
        parts.join("::")
    }

    /// Generate candidate module-qualified function names for `anode context` given a symbol at cursor.
    pub fn candidate_function_names(&self, file_path: &Path, raw_symbol: &str) -> Vec<String> {
        if let Some(stripped) = raw_symbol.strip_prefix("crate::") {
            return vec![stripped.to_owned()];
        }
        let module = self.module_path_for_file(file_path);
        if module.is_empty() {
            vec![raw_symbol.to_owned()]
        } else {
            let qualified = format!("{module}::{raw_symbol}");
            if raw_symbol.starts_with(&format!("{module}::")) {
                vec![raw_symbol.to_owned(), qualified]
            } else {
                vec![qualified, raw_symbol.to_owned()]
            }
        }
    }

    /// Walk `mod <name>;` declarations on disk starting from `self.entry_file` to find
    /// the set of canonical file paths currently loaded in the compiler's module graph.
    pub fn discover_reachable_disk_files(&self) -> BTreeSet<PathBuf> {
        let mut visited = BTreeSet::new();
        self.walk_disk_module(&self.entry_file, true, 0, &mut visited);
        for (_, _, entry) in self.dependency_roots() {
            self.walk_disk_module(&entry, true, 0, &mut visited);
        }
        visited
    }

    fn walk_disk_module(
        &self,
        path: &Path,
        is_root: bool,
        depth: usize,
        visited: &mut BTreeSet<PathBuf>,
    ) {
        if depth > 32 || visited.len() >= 128 {
            return;
        }
        let norm = normalize_path(path);
        if !norm.is_file() || !visited.insert(norm.clone()) {
            return;
        }
        let Ok(source) = fs::read_to_string(&norm) else {
            return;
        };
        let Some(ext) = norm.extension() else {
            return;
        };
        let dir = if is_root {
            norm.parent().unwrap_or(&self.root).to_path_buf()
        } else {
            norm.with_extension("")
        };
        for child_mod in scan_mod_declarations(&source) {
            let child_path = dir.join(child_mod).with_extension(ext);
            self.walk_disk_module(&child_path, false, depth + 1, visited);
        }
    }
}

fn package_manifest(root: &Path) -> PathBuf {
    let anode = root.join("Anode.toml");
    if anode.is_file() {
        anode
    } else {
        root.join("Zenith.toml")
    }
}

fn manifest_code(raw: &str) -> &str {
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
    raw[..end].trim()
}

/// Read `build.entry` from `Anode.toml` or `Zenith.toml`, falling back to `src/main.an` or `src/main.zn`.
pub fn parse_manifest_entry(manifest_path: &Path) -> PathBuf {
    let is_anode = manifest_path.file_name().is_some_and(|n| n == "Anode.toml");
    let default_entry = if is_anode {
        "src/main.an"
    } else {
        "src/main.zn"
    };

    let Ok(text) = fs::read_to_string(manifest_path) else {
        return PathBuf::from(default_entry);
    };

    let mut section = "";
    for raw in text.lines() {
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
        let line = raw[..end].trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            section = line;
            continue;
        }
        if section == "[build]" {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "entry" {
                    if let Some(unquoted) =
                        v.trim().strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                    {
                        return PathBuf::from(unquoted);
                    }
                }
            }
        }
    }

    PathBuf::from(default_entry)
}

/// Lightweight scan for top-level `mod <ident>;` declarations in an Anode source file.
pub fn scan_mod_declarations(source: &str) -> Vec<String> {
    let mut mods = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut depth = 0usize;
    let mut stage = 0;
    let mut name = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'"' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == b'"' {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
                stage = 0;
            }
            b'{' => {
                depth += 1;
                stage = 0;
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                stage = 0;
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                let word = &source[start..index];
                if depth == 0 && word == "mod" {
                    stage = 1;
                } else if stage == 1 {
                    name = word.into();
                    stage = 2;
                } else {
                    stage = 0;
                }
            }
            b';' if stage == 2 && depth == 0 => {
                mods.push(std::mem::take(&mut name));
                stage = 0;
                index += 1;
            }
            _ => {
                stage = 0;
                index += 1;
            }
        }
    }
    mods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_and_verbatim_path_normalization() {
        let verbatim = Path::new(r"\\?\c:\Users\dev\examples\notebook\src\search.an");
        let norm1 = strip_verbatim_and_normalize_drive(verbatim);
        assert_eq!(
            norm1,
            PathBuf::from(r"C:\Users\dev\examples\notebook\src\search.an")
        );

        let vscode_uri = "file:///c%3A/Users/dev/examples/notebook/src/search.an";
        let from_uri = uri_to_path(vscode_uri).unwrap();
        assert_eq!(from_uri, norm1);

        let roundtrip_uri = path_to_uri(&norm1);
        assert_eq!(
            roundtrip_uri,
            "file:///C:/Users/dev/examples/notebook/src/search.an"
        );
        assert_eq!(uri_to_path(&roundtrip_uri).unwrap(), norm1);
    }

    #[test]
    fn test_candidate_function_names() {
        let layout = ProjectLayout {
            root: PathBuf::from(r"C:\proj"),
            agent_target: PathBuf::from(r"C:\proj"),
            entry_file: PathBuf::from(r"C:\proj\src\main.an"),
            managed: true,
        };
        let main_file = PathBuf::from(r"C:\proj\src\main.an");
        let search_file = PathBuf::from(r"C:\proj\src\search.an");
        let nested_file = PathBuf::from(r"C:\proj\src\search\order.an");

        assert_eq!(layout.module_path_for_file(&main_file), "");
        assert_eq!(layout.module_path_for_file(&search_file), "search");
        assert_eq!(layout.module_path_for_file(&nested_file), "search::order");

        assert_eq!(
            layout.candidate_function_names(&main_file, "search::matching"),
            vec!["search::matching"]
        );
        assert_eq!(
            layout.candidate_function_names(&search_file, "matching"),
            vec!["search::matching", "matching"]
        );
        assert_eq!(
            layout.candidate_function_names(&search_file, "crate::helper::run"),
            vec!["helper::run"]
        );
    }
}
