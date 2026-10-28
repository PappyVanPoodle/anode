//! Project build storage is bounded by output variants, not source-edit history.
use crate::{backend, project::Project, Error, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn io(e: std::io::Error) -> Error {
    Error::new(0, e.to_string())
}
fn managed_directory(project: &Project) -> Result<PathBuf> {
    if !project.managed {
        return Err(Error::new(
            0,
            "storage cleanup requires a Zenith.toml project",
        ));
    }
    let base = project.root.join(".zenith");
    let directory = base.join("build");
    for path in [&base, &directory] {
        if path.exists() && path.canonicalize().map_err(io)? != *path {
            return Err(Error::new(
                0,
                ".zenith/build must not be redirected through a link or junction",
            ));
        }
    }
    Ok(directory)
}
pub fn prepare(project: &Project) -> Result<()> {
    let directory = managed_directory(project)?;
    fs::create_dir_all(&directory).map_err(io)?;
    managed_directory(project)?;
    Ok(())
}
fn bytes(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |m| if m.is_file() { m.len() } else { 0 })
}
pub fn report(project: &Project) -> Result<String> {
    let directory = managed_directory(project)?;
    let mut executables = 0;
    let mut libraries = 0;
    let mut receipts = 0;
    for testing in [false, true] {
        for optimize in [false, true] {
            for sanitize in [false, true] {
                let exe = project.output(testing, optimize, sanitize);
                executables += bytes(&exe);
                receipts += bytes(&exe.with_extension("zn-build"));
            }
        }
    }
    for optimize in [false, true] {
        let output = project.output(false, optimize, false).with_extension("lib");
        for path in [
            &output,
            &output.with_extension("h"),
            &output.with_extension("rs"),
        ] {
            libraries += bytes(path);
        }
        receipts += bytes(&output.with_extension("zn-lib-build"));
    }
    let mut managed_bytes = 0u64;
    let mut retained_objects = 0u64;
    if directory.exists() {
        for entry in fs::read_dir(&directory).map_err(io)? {
            let entry = entry.map_err(io)?;
            let metadata = entry.metadata().map_err(io)?;
            if metadata.is_file() {
                managed_bytes += metadata.len();
                if entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("obj"))
                {
                    retained_objects += metadata.len();
                }
            }
        }
    }
    let additional = managed_bytes.saturating_sub(executables + libraries + receipts);
    let cache = backend::cache_root();
    let shared: u64 = [
        "runtime.obj",
        "runtime.identity",
        "runtime-asan.obj",
        "runtime-asan.identity",
        "runtime.lock",
        "toolchain.txt",
    ]
    .iter()
    .map(|name| bytes(&cache.join(name)))
    .sum();
    let modules = crate::object_cache::size(&cache.join("modules"));
    Ok(format!("Project: {}\nBuild output: {}\nExecutables: {executables} bytes\nLibraries and generated bindings: {libraries} bytes\nBuild receipts: {receipts} bytes\nAdditional artifacts (including reload patches): {additional} bytes\nTotal managed build files: {managed_bytes} bytes\nRetained per-project intermediate objects: {retained_objects} bytes\nShared object/baked-executable cache: {modules} bytes\nCombined artifact budget for this project: {} bytes (0 = disabled; --cache-mb overrides)\nShared native runtime/toolchain cache: {shared} bytes\nShared cache: {}\nUp to eight named executables (program/tests x fast/release x normal/ASan) and two static libraries with bindings, overwritten on rebuild. Native objects, C modules and baked EXEs share one LRU budget; builds using it enforce their configured cap. Explicit reload DLL/object/header/proof generations are published artifacts outside that cache budget, retained for host code lifetime; clean does not remove them. Bake status/lock files are small per-project metadata. Installed toolchains and the Rust compiler-development cache are separate.", project.name, directory.display(), project.cache_bytes, cache.display()))
}
pub fn clean(project: &Project) -> Result<(u64, usize)> {
    let directory = managed_directory(project)?;
    if !directory.exists() {
        return Ok((0, 0));
    }
    let mut removed = 0;
    let mut count = 0;
    for testing in [false, true] {
        for optimize in [false, true] {
            for sanitize in [false, true] {
                let exe = project.output(testing, optimize, sanitize);
                let lock_path = exe.with_extension("zn-lock");
                let lock = fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(&lock_path)
                    .map_err(io)?;
                lock.try_lock()
                    .map_err(|e| Error::new(0, format!("build is in progress: {e}")))?;
                let mut artifacts = vec![exe.clone(), exe.with_extension("zn-build")];
                if !testing && !sanitize {
                    artifacts.extend(
                        ["lib", "h", "rs", "zn-lib-build"].map(|ext| exe.with_extension(ext)),
                    );
                }
                for path in &artifacts {
                    if path.exists() {
                        let canonical = path.canonicalize().map_err(io)?;
                        if canonical.parent() != Some(directory.as_path()) {
                            return Err(Error::new(0, "refusing to remove redirected output"));
                        }
                        let size = bytes(path);
                        fs::remove_file(path).map_err(io)?;
                        removed += size;
                        count += 1;
                    }
                }
                // Keep the zero-byte lock file: deleting it would race a waiting builder.
            }
        }
    }
    Ok((removed, count))
}
