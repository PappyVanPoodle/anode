use crate::{Error, Result};
use std::{
    env, fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
pub struct Toolchain {
    pub compiler: PathBuf,
    pub includes: Vec<PathBuf>,
    pub libs: Vec<PathBuf>,
}
pub fn cache_root() -> PathBuf {
    env::var_os("ZN_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                env::var_os("LOCALAPPDATA").unwrap_or_else(|| env::temp_dir().into_os_string()),
            )
            .join("ZenithLanguage/native-cache")
        })
}
pub fn refresh_toolchain() -> Result<()> {
    let path = cache_root().join("toolchain.txt");
    if path.exists() {
        fs::remove_file(path).map_err(io)?;
    }
    Ok(())
}
fn latest(root: &Path) -> Result<PathBuf> {
    let mut dirs = fs::read_dir(root)
        .map_err(|e| Error::new(0, format!("{}: {e}", root.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect::<Vec<_>>();
    dirs.sort_by_key(|p| {
        p.file_name()
            .unwrap()
            .to_string_lossy()
            .split('.')
            .map(|s| s.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    dirs.pop()
        .ok_or_else(|| Error::new(0, format!("no toolchain under {}", root.display())))
}
pub fn discover() -> Result<Toolchain> {
    if !cfg!(target_os = "windows") {
        return Err(Error::new(
            0,
            "native backend currently supports Windows x64 with MSVC",
        ));
    }
    let cached = cache_root().join("toolchain.txt");
    if let Ok(text) = fs::read_to_string(&cached) {
        let p: Vec<_> = text.lines().map(PathBuf::from).collect();
        if p.len() == 8 && p[0].is_file() && p[1..].iter().all(|p| p.is_dir()) {
            return Ok(Toolchain {
                compiler: p[0].clone(),
                includes: p[1..5].to_vec(),
                libs: p[5..].to_vec(),
            });
        }
    }
    let program = PathBuf::from(
        env::var_os("ProgramFiles(x86)").unwrap_or_else(|| "C:/Program Files (x86)".into()),
    );
    let vswhere = program.join("Microsoft Visual Studio/Installer/vswhere.exe");
    let result = Command::new(vswhere)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ])
        .output()
        .map_err(|e| Error::new(0, format!("MSVC discovery failed: {e}")))?;
    if !result.status.success() {
        return Err(Error::new(0, "vswhere failed"));
    }
    let root = String::from_utf8_lossy(&result.stdout).trim().to_owned();
    if root.is_empty() {
        return Err(Error::new(
            0,
            "install the Visual Studio C++ build tools and Windows SDK first",
        ));
    }
    let msvc = latest(&PathBuf::from(root).join("VC/Tools/MSVC"))?;
    let sdk = program.join("Windows Kits/10");
    let include = latest(&sdk.join("Include"))?;
    let lib = sdk.join("Lib").join(include.file_name().unwrap());
    let t = Toolchain {
        compiler: msvc.join("bin/Hostx64/x64/cl.exe"),
        includes: vec![
            msvc.join("include"),
            include.join("ucrt"),
            include.join("shared"),
            include.join("um"),
        ],
        libs: vec![
            msvc.join("lib/x64"),
            lib.join("ucrt/x64"),
            lib.join("um/x64"),
        ],
    };
    if !t.compiler.is_file() || t.includes.iter().chain(&t.libs).any(|p| !p.is_dir()) {
        return Err(Error::new(0, "incomplete MSVC or Windows SDK installation"));
    }
    fs::create_dir_all(cache_root()).map_err(io)?;
    let text = std::iter::once(&t.compiler)
        .chain(&t.includes)
        .chain(&t.libs)
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(cached, text).map_err(io)?;
    Ok(t)
}
struct Scratch {
    path: PathBuf,
    root: PathBuf,
}
impl Scratch {
    fn new() -> Result<Self> {
        let root = env::temp_dir().join("zenith-language-scratch");
        fs::create_dir_all(&root).map_err(io)?;
        let root = root.canonicalize().map_err(io)?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = root.join(format!("zn-{}-{stamp}", std::process::id()));
        fs::create_dir(&path).map_err(io)?;
        Ok(Self { path, root })
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(path) = self.path.canonicalize() {
            if path.parent() == Some(self.root.as_path())
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("zn-"))
            {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}
fn io(e: std::io::Error) -> Error {
    Error::new(0, e.to_string())
}
pub(crate) fn publish_library_artifact(path: &Path, bytes: &[u8]) -> Result<()> {
    // Replace a directory entry rather than truncating through an existing
    // symlink or hardlink. A failed publication never earns a valid receipt.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("zn-publish-{}-{stamp}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(io)?;
    use std::io::Write;
    if let Err(error) = file.write_all(bytes) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(io(error));
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io(error));
    }
    Ok(())
}
#[derive(Debug)]
pub struct Build {
    pub output: PathBuf,
    pub bytes: u64,
    pub backend_ms: f64,
    pub runtime_rebuilt: bool,
    pub sanitizer_runtime_dir: Option<PathBuf>,
    pub modules_reused: usize,
    pub modules_compiled: usize,
}
#[derive(Clone, Debug, Default)]
pub struct BuildOptions {
    pub optimize: bool,
    pub sanitize: bool,
    pub force: bool,
    pub cache_bytes: u64,
    /// Explicit trusted native provider archives. No shell/linker fragments.
    pub ffi_libraries: Vec<PathBuf>,
}
impl Build {
    pub fn command(&self) -> Result<Command> {
        let mut command = Command::new(&self.output);
        if let Some(dir) = &self.sanitizer_runtime_dir {
            let mut paths = vec![dir.clone()];
            paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
            command.env(
                "PATH",
                env::join_paths(paths).map_err(|e| Error::new(0, e.to_string()))?,
            );
        }
        Ok(command)
    }
}
#[derive(Debug)]
pub struct TestCaseReport {
    pub name: String,
    pub output: PathBuf,
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
#[derive(Debug)]
pub struct TestReport {
    /// One dispatcher artifact is shared by independent test processes.
    pub build: Build,
    pub reused: bool,
    /// Source order, independent of process completion order.
    pub cases: Vec<TestCaseReport>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}
impl TestReport {
    pub fn success(&self) -> bool {
        self.cases.iter().all(|case| case.status.success())
    }
}
/// Check and compile exactly one private dispatcher, then execute independent
/// child processes with bounded concurrency. Captured bytes and exit statuses
/// are never interpreted as passing merely because compilation succeeded.
pub fn run_tests_with_jobs(
    project: &crate::project::Project,
    output: &Path,
    options: &BuildOptions,
    native: bool,
    jobs: usize,
) -> Result<TestReport> {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    if !(1..=16).contains(&jobs) {
        return Err(Error::new(0, "test jobs must be from 1 to 16"));
    }
    let directory = env::current_dir().map_err(io)?;
    let (program, names) = project.checked_test_dispatcher()?;
    let (build, reused) = if native {
        build_fast(&program, output, options)?
    } else {
        let parts = crate::project::Project::c_parts(&program, options.cache_bytes == 0);
        let artifact =
            crate::project::Project::generated_input(parts, names.len(), options.cache_bytes > 0);
        build_reusing(&artifact.c, &artifact.units, output, options)?
    };
    let next = AtomicUsize::new(0);
    let (send, receive) = mpsc::channel();
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        for _ in 0..jobs.min(names.len()) {
            let (send, next, build, names, directory) =
                (send.clone(), &next, &build, &names, &directory);
            workers.push(scope.spawn(move || loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= names.len() {
                    break;
                }
                let result = (|| {
                    let child = build
                        .command()?
                        .arg(index.to_string())
                        .current_dir(directory)
                        .output()
                        .map_err(|e| {
                            Error::new(0, format!("test {} failed to start: {e}", names[index]))
                        })?;
                    Ok(TestCaseReport {
                        name: names[index].clone(),
                        output: build.output.clone(),
                        status: child.status,
                        stdout: child.stdout,
                        stderr: child.stderr,
                    })
                })();
                if send.send((index, result)).is_err() {
                    break;
                }
            }));
        }
        for worker in workers {
            worker
                .join()
                .map_err(|_| Error::new(0, "test process worker panicked"))?;
        }
        Ok(())
    })?;
    drop(send);
    let mut ordered: Vec<Option<Result<TestCaseReport>>> =
        std::iter::repeat_with(|| None).take(names.len()).collect();
    for (index, result) in receive {
        ordered[index] = Some(result);
    }
    let cases = ordered
        .into_iter()
        .map(|case| {
            case.ok_or_else(|| Error::new(0, "test process worker did not report a result"))?
        })
        .collect::<Result<Vec<_>>>()?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for case in &cases {
        stdout.extend_from_slice(format!("test {}\n", case.name).as_bytes());
        stdout.extend_from_slice(&case.stdout);
        if !case.status.success() {
            stderr.extend_from_slice(
                format!("test {} failed: {}\n", case.name, case.status).as_bytes(),
            );
        }
        stderr.extend_from_slice(&case.stderr);
    }
    let failures = cases.iter().filter(|case| !case.status.success()).count();
    stdout.extend_from_slice(
        if failures == 0 {
            format!("{} tests passed\n", cases.len())
        } else {
            format!(
                "{failures} tests failed; {} passed\n",
                cases.len() - failures
            )
        }
        .as_bytes(),
    );
    Ok(TestReport {
        build,
        reused,
        cases,
        stdout,
        stderr,
    })
}
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    path: PathBuf,
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
    #[cfg(windows)]
    identity: (u32, u64),
    #[cfg(unix)]
    identity: (u64, u64),
}
impl FileStamp {
    fn from_file(path: PathBuf, file: &fs::File) -> Result<Self> {
        let metadata = file.metadata().map_err(io)?;
        if !metadata.is_file() {
            return Err(Error::new(0, "compiler identity requires a regular file"));
        }
        Ok(Self {
            path,
            len: metadata.len(),
            modified: metadata.modified().map_err(io)?,
            created: metadata.created().ok(),
            #[cfg(windows)]
            identity: {
                use std::os::windows::io::AsRawHandle;
                #[link(name = "kernel32")]
                extern "system" {
                    fn GetFileInformationByHandle(
                        handle: *mut std::ffi::c_void,
                        info: *mut u32,
                    ) -> i32;
                }
                // BY_HANDLE_FILE_INFORMATION is thirteen DWORDs. Volume and
                // file index detect replacement even with restored timestamps.
                let mut info = [0u32; 13];
                if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) }
                    == 0
                {
                    return Err(io(std::io::Error::last_os_error()));
                }
                (info[7], (u64::from(info[11]) << 32) | u64::from(info[12]))
            },
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        })
    }
    fn read(path: &Path) -> Result<Self> {
        let path = path.canonicalize().map_err(io)?;
        let file = fs::File::open(&path).map_err(io)?;
        Self::from_file(path, &file)
    }
}
struct CachedDigest {
    stamp: FileStamp,
    digest: (u64, String),
}
/// Metadata-validated digest cache. Exposed for storage diagnostics and tests
/// that must never modify the running compiler executable.
#[doc(hidden)]
#[derive(Default)]
pub struct FileDigestCache {
    cached: Mutex<Option<CachedDigest>>,
    reads: AtomicU64,
}
impl FileDigestCache {
    pub fn digest(&self, path: &Path) -> Result<(u64, String)> {
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| Error::new(0, "compiler identity lock poisoned"))?;
        let stamp = match FileStamp::read(path) {
            Ok(stamp) => stamp,
            Err(error) => {
                *cached = None;
                return Err(error);
            }
        };
        if let Some(old) = cached.as_ref().filter(|old| old.stamp == stamp) {
            return Ok(old.digest.clone());
        }
        // Invalidate before any fallible open/read/hash. A failed refresh must
        // never leave an old identity available to another request.
        *cached = None;
        let mut file = fs::File::open(&stamp.path).map_err(io)?;
        let opened = FileStamp::from_file(stamp.path.clone(), &file)?;
        if opened != stamp {
            return Err(Error::new(0, "compiler changed before identity read"));
        }
        self.reads.fetch_add(1, Ordering::Relaxed);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io)?;
        let digest = (hash_bytes(&bytes), compiler_sha256(&bytes));
        let after = FileStamp::from_file(stamp.path.clone(), &file)?;
        if after != stamp || FileStamp::read(path)? != stamp || bytes.len() as u64 != stamp.len {
            return Err(Error::new(0, "compiler changed during identity read"));
        }
        *cached = Some(CachedDigest {
            stamp,
            digest: digest.clone(),
        });
        Ok(digest)
    }
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
}
static COMPILER_EXE_DIGEST: OnceLock<FileDigestCache> = OnceLock::new();
/// Return (legacy DefaultHasher digest, lowercase SHA-256 hex) of the same
/// compiler executable bytes. Every request validates current_exe's canonical
/// path, length, modified/creation times and platform file identity. Unchanged
/// concurrent requests read once; changed metadata and transient failures
/// invalidate the successful snapshot and permit a later retry.
pub fn compiler_exe_digest() -> Result<(u64, String)> {
    let cache = COMPILER_EXE_DIGEST.get_or_init(FileDigestCache::default);
    match env::current_exe() {
        Ok(path) => cache.digest(&path),
        Err(error) => {
            *cache
                .cached
                .lock()
                .map_err(|_| Error::new(0, "compiler identity lock poisoned"))? = None;
            Err(io(error))
        }
    }
}
#[doc(hidden)]
pub fn compiler_exe_digest_read_count() -> u64 {
    COMPILER_EXE_DIGEST
        .get()
        .map_or(0, FileDigestCache::read_count)
}
fn compiler_sha256(bytes: &[u8]) -> String {
    // The exact compiler SHA covers megabytes on every fresh process. Use the
    // OS provider's accelerated implementation, retaining the portable exact
    // algorithm if the provider is unavailable or cannot accept this length.
    #[cfg(windows)]
    if let Some(digest) = windows_sha256(bytes) {
        return digest;
    }
    sha256(bytes)
}
#[cfg(windows)]
fn windows_sha256(bytes: &[u8]) -> Option<String> {
    use std::ffi::c_void;
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptOpenAlgorithmProvider(
            algorithm: *mut *mut c_void,
            id: *const u16,
            implementation: *const u16,
            flags: u32,
        ) -> i32;
        fn BCryptHash(
            algorithm: *mut c_void,
            secret: *const u8,
            secret_len: u32,
            input: *const u8,
            input_len: u32,
            output: *mut u8,
            output_len: u32,
        ) -> i32;
        fn BCryptCloseAlgorithmProvider(algorithm: *mut c_void, flags: u32) -> i32;
    }
    let len = u32::try_from(bytes.len()).ok()?;
    let mut algorithm = std::ptr::null_mut();
    const SHA256: [u16; 7] = [83, 72, 65, 50, 53, 54, 0];
    // All pointers refer to live slices/stack storage for this synchronous
    // call. CNG reads exactly len bytes and writes the 32-byte SHA-256 result.
    if unsafe { BCryptOpenAlgorithmProvider(&mut algorithm, SHA256.as_ptr(), std::ptr::null(), 0) }
        < 0
    {
        return None;
    }
    let mut output = [0u8; 32];
    let status = unsafe {
        BCryptHash(
            algorithm,
            std::ptr::null(),
            0,
            bytes.as_ptr(),
            len,
            output.as_mut_ptr(),
            output.len() as u32,
        )
    };
    unsafe { BCryptCloseAlgorithmProvider(algorithm, 0) };
    (status >= 0).then(|| output.iter().map(|b| format!("{b:02x}")).collect())
}
#[derive(Debug)]
struct FfiLibrary {
    path: PathBuf,
    bytes: Vec<u8>,
    sha256: String,
}
fn ffi_libraries(paths: &[PathBuf]) -> Result<Vec<FfiLibrary>> {
    paths
        .iter()
        .map(|path| {
            if path
                .extension()
                .is_none_or(|e| !e.eq_ignore_ascii_case("lib"))
            {
                return Err(
                    Error::new(0, "FFI provider must be an explicit .lib file").in_file(path)
                );
            }
            let actual = path
                .canonicalize()
                .map_err(|e| Error::new(0, format!("FFI provider: {e}")).in_file(path))?;
            let bytes = fs::read(&actual).map_err(io)?;
            crate::coff_archive::validate_provider(&bytes).map_err(|e| e.in_file(&actual))?;
            Ok(FfiLibrary {
                path: actual,
                sha256: sha256(&bytes),
                bytes,
            })
        })
        .collect()
}
fn hash_ffi(libraries: &[FfiLibrary], key: &mut impl Hasher) {
    for library in libraries {
        library.path.hash(key);
        library.sha256.hash(key);
    }
}
fn stage_ffi(libraries: &[FfiLibrary], scratch: &Path) -> Result<Vec<PathBuf>> {
    libraries
        .iter()
        .enumerate()
        .map(|(i, library)| {
            let name = PathBuf::from(format!("ffi-provider-{i}.lib"));
            let path = scratch.join(&name);
            fs::write(&path, &library.bytes).map_err(io)?;
            // Commands run in scratch. In particular cl.exe must not interpret a
            // canonical Windows \\?\ path's question mark as input glob syntax.
            Ok(name)
        })
        .collect()
}
/// Stable SHA-256 identities cover source snapshots and the exact native bytes
/// actually linked/published. This implementation adds no runtime dependency.
pub(crate) fn sha256(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&(bytes.len() as u64).wrapping_mul(8).to_be_bytes());
    for block in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add((e & f) ^ (!e & g))
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let t2 = s0.wrapping_add((a & b) ^ (a & c) ^ (b & c));
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}
const C_COMMON_FLAGS: &[&str] = &[
    "/nologo",
    "/TC",
    "/std:c11",
    "/W3",
    "/utf-8",
    "/MD",
    "/GS",
    "/Gy",
    "/Gw",
    "/GF",
    "/Zc:inline",
    "/fp:precise",
];
const LINK_FLAGS: &[&str] = &[
    "/INCREMENTAL:NO",
    "/DEBUG:NONE",
    "/OPT:REF",
    "/OPT:ICF",
    "/DYNAMICBASE",
    "/NXCOMPAT",
];
fn hash_codegen_flags(key: &mut impl Hasher) {
    "anode-native-codegen-v028-xmm-static-text-pool-ltcg".hash(key);
    native_isa_features().hash(key);
    C_COMMON_FLAGS.hash(key);
    LINK_FLAGS.hash(key);
    [
        "/O2",
        "/Ob1",
        "/Od",
        "/DZN_OUTLINE_ARITHMETIC",
        "/fsanitize=address",
        "/GL",
        "/LTCG",
    ]
    .hash(key);
}
/// Stable private recipe bits shared with native lowering. Never enable an
/// optional instruction unless this exact compiling host reports support.
pub(crate) fn native_isa_features() -> u8 {
    #[cfg(target_arch = "x86_64")]
    {
        u8::from(std::is_x86_feature_detected!("popcnt"))
            | (u8::from(std::is_x86_feature_detected!("lzcnt")) << 1)
            | (u8::from(std::is_x86_feature_detected!("bmi1")) << 2)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}
pub(crate) fn native_codegen_receipt() -> serde_json::Value {
    serde_json::json!({
        "schema":"anode.native-codegen.v2",
        "private_abi":"anode-v028-xmm-static-text-pool",
        "c_flags":C_COMMON_FLAGS,
        "link_flags":LINK_FLAGS,
        "optimized_flags":["/O2","/Ob1"],
        "debug_flags":["/Od","/DZN_OUTLINE_ARITHMETIC"],
        "sanitizer_flags":["/fsanitize=address"],
        "ltcg_c_flags":["/GL"],
        "ltcg_link_flags":["/LTCG"],
        "ltcg_rule":"optimized non-ASan C/baked/library/reload paths only; ordinary COFF runtime on Cranelift paths",
        "native_isa_mask":native_isa_features(),
        "native_isa_bits":{"popcnt":1,"lzcnt":2,"bmi1":4},
        "function_addresses":"callable tokens; identical bodies may share an address",
        "native_contract":"trusted FFI providers and hosts must not require distinct addresses for identical functions or pooled immutable strings"
    })
}
fn compiler(
    tool: &Toolchain,
    directory: &Path,
    optimize: bool,
    sanitize: bool,
    ltcg: bool,
) -> Command {
    let mut c = Command::new(&tool.compiler);
    c.current_dir(directory)
        .args(C_COMMON_FLAGS)
        .arg(if optimize { "/O2" } else { "/Od" });
    if optimize {
        c.arg("/Ob1");
    } else {
        c.arg("/DZN_OUTLINE_ARITHMETIC");
    }
    if sanitize {
        c.arg("/fsanitize=address");
    }
    if ltcg && optimize && !sanitize {
        c.arg("/GL");
    }
    for p in &tool.includes {
        c.arg(format!("/I{}", p.display()));
    }
    c.env_remove("CL")
        .env_remove("_CL_")
        .env_remove("LINK")
        .env_remove("INCLUDE")
        .env_remove("LIB");
    c
}
fn checked_command(c: &mut Command) -> Result<()> {
    let result = c.output().map_err(io)?;
    if !result.status.success() {
        return Err(Error::new(
            0,
            format!(
                "native backend failed:\n{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            ),
        ));
    }
    Ok(())
}
fn runtime(tool: &Toolchain, scratch: &Path, sanitize: bool, ltcg: bool) -> Result<bool> {
    let root = cache_root();
    fs::create_dir_all(&root).map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("runtime.lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let meta = fs::metadata(&tool.compiler).map_err(io)?;
    let provenance = format!(
        "anode-runtime-provenance-v028\n{tool:?}\n{:?}\n{}\ncompiler={}\nsanitize={sanitize}\nltcg={ltcg}\noptimize=true\n{}",
        meta.modified().map_err(io)?,
        meta.len(),
        compiler_exe_digest()?.1,
        native_codegen_receipt(),
    );
    static SOURCES: OnceLock<(String, String)> = OnceLock::new();
    let (source, header) = SOURCES.get_or_init(|| {
        (
            sha256(include_bytes!("runtime.c")),
            sha256(include_bytes!("runtime.h")),
        )
    });
    let identity = format!(
        "anode-runtime-v028\nc={source}\nh={header}\np={}\n",
        sha256(provenance.as_bytes())
    );
    let name = if sanitize {
        "runtime-asan"
    } else if ltcg {
        "runtime-ltcg"
    } else {
        "runtime"
    };
    let obj = root.join(format!("{name}.obj"));
    let identity_file = root.join(format!("{name}.identity"));
    let expected = fs::read(&obj)
        .ok()
        .filter(|b| crate::coff_archive::object_payload(b, ltcg && !sanitize))
        .map(|b| format!("{:016x}\n{identity}", hash_bytes(&b)));
    let valid = expected
        .is_some_and(|v| fs::read_to_string(&identity_file).is_ok_and(|actual| actual == v));
    if !valid {
        fs::write(scratch.join("runtime.c"), include_str!("runtime.c")).map_err(io)?;
        fs::write(scratch.join("runtime.h"), include_str!("runtime.h")).map_err(io)?;
        checked_command(compiler(tool, scratch, true, sanitize, ltcg).args([
            "/c",
            "runtime.c",
            "/Foruntime.obj",
        ]))?;
        let bytes = fs::read(scratch.join("runtime.obj")).map_err(io)?;
        publish_library_artifact(&obj, &bytes)?;
        publish_library_artifact(
            &identity_file,
            format!("{:016x}\n{identity}", hash_bytes(&bytes)).as_bytes(),
        )?;
    }
    fs::copy(obj, scratch.join("runtime.obj")).map_err(io)?;
    Ok(!valid)
}
fn cached_object(
    cache: &crate::object_cache::Cache,
    key: u64,
    destination: &Path,
    ltcg: bool,
) -> Result<bool> {
    let Some(bytes) = cache
        .get_bytes(key)?
        .filter(|b| crate::coff_archive::object_payload(b, ltcg))
    else {
        return Ok(false);
    };
    fs::write(destination, bytes).map_err(io)?;
    Ok(true)
}
pub fn build(c: &str, output: &Path, optimize: bool) -> Result<Build> {
    build_checked(c, output, optimize, false)
}
/// Whole-output reuse, not an intermediate/object cache. The caller still runs
/// all frontend checks. One sub-128-byte receipt per output is overwritten.
pub fn build_reusing(
    c: &str,
    units: &[(String, String)],
    output: &Path,
    options: &BuildOptions,
) -> Result<(Build, bool)> {
    let (optimize, sanitize, force, cache_bytes) = (
        options.optimize,
        options.sanitize,
        options.force,
        options.cache_bytes,
    );
    let providers = ffi_libraries(&options.ffi_libraries)?;
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(Error::new(0, "native output must have an .exe extension"));
    }
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "output has no parent"))?,
    )
    .map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.with_extension("zn-lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let tool = discover()?;
    let mut key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut key);
    hash_ffi(&providers, &mut key);
    c.hash(&mut key);
    optimize.hash(&mut key);
    sanitize.hash(&mut key);
    (cache_bytes > 0).hash(&mut key);
    include_str!("runtime.c").hash(&mut key);
    include_str!("runtime.h").hash(&mut key);
    compiler_exe_digest()?.hash(&mut key);
    format!("{tool:?}").hash(&mut key);
    for path in [&tool.compiler, &tool.compiler.with_file_name("link.exe")] {
        let metadata = fs::metadata(path).map_err(io)?;
        metadata.len().hash(&mut key);
        metadata.modified().map_err(io)?.hash(&mut key);
    }
    let identity = format!(
        "zenith-build-v1\n{:016x}\nflags={}",
        key.finish(),
        if optimize && !sanitize {
            "/GL,/LTCG"
        } else if sanitize {
            "/fsanitize=address"
        } else {
            "ordinary"
        }
    );
    let receipt = output.with_extension("zn-build");
    if !force {
        if let Ok(bytes) = fs::read(&output) {
            let expected = format!("{identity}\n{:016x}\n", hash_bytes(&bytes));
            if fs::read_to_string(&receipt).is_ok_and(|s| s == expected) {
                if cache_bytes > 0 {
                    drop(crate::object_cache::Cache::open(
                        &cache_root().join("modules"),
                        cache_bytes,
                    )?);
                }
                let sanitizer_runtime_dir = if sanitize {
                    let directory = tool.compiler.parent().unwrap().to_owned();
                    if !directory.join("clang_rt.asan_dynamic-x86_64.dll").is_file() {
                        return Err(Error::new(
                            0,
                            "MSVC AddressSanitizer runtime is not installed",
                        ));
                    }
                    Some(directory)
                } else {
                    None
                };
                return Ok((
                    Build {
                        output,
                        bytes: bytes.len() as u64,
                        backend_ms: 0.0,
                        runtime_rebuilt: false,
                        sanitizer_runtime_dir,
                        modules_reused: 0,
                        modules_compiled: 0,
                    },
                    true,
                ));
            }
        }
    }
    let build = if cache_bytes > 0 {
        build_modules(units, &output, options, &providers)?
    } else {
        build_checked_impl(c, &output, optimize, sanitize, &providers)?
    };
    let expected = format!(
        "{identity}\n{:016x}\n",
        hash_bytes(&fs::read(&output).map_err(io)?)
    );
    publish_library_artifact(&receipt, expected.as_bytes())?;
    Ok((build, false))
}
pub fn build_checked(c: &str, output: &Path, optimize: bool, sanitize: bool) -> Result<Build> {
    build_checked_impl(c, output, optimize, sanitize, &[])
}
pub fn build_checked_with_options(c: &str, output: &Path, options: &BuildOptions) -> Result<Build> {
    let providers = ffi_libraries(&options.ffi_libraries)?;
    build_checked_impl(c, output, options.optimize, options.sanitize, &providers)
}
fn build_checked_impl(
    c: &str,
    output: &Path,
    optimize: bool,
    sanitize: bool,
    providers: &[FfiLibrary],
) -> Result<Build> {
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(Error::new(0, "native output must have an .exe extension"));
    }
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    let tool = discover()?;
    let sanitizer_runtime_dir = if sanitize {
        let directory = tool
            .compiler
            .parent()
            .ok_or_else(|| Error::new(0, "missing compiler directory"))?;
        if !directory.join("clang_rt.asan_dynamic-x86_64.dll").is_file() {
            return Err(Error::new(
                0,
                "MSVC AddressSanitizer runtime is not installed",
            ));
        }
        Some(directory.to_owned())
    } else {
        None
    };
    let scratch = Scratch::new()?;
    let source = scratch.path.join("program.c");
    let exe = scratch.path.join("program.exe");
    fs::write(&source, c).map_err(io)?;
    let start = Instant::now();
    let runtime_rebuilt = runtime(&tool, &scratch.path, sanitize, optimize && !sanitize)?;
    let providers = stage_ffi(providers, &scratch.path)?;
    let mut command = compiler(&tool, &scratch.path, optimize, sanitize, true);
    command
        .arg("program.c")
        .arg("/Foprogram.obj")
        .arg("/Feprogram.exe")
        .arg("/link")
        .arg("runtime.obj")
        .args(&providers)
        .args(LINK_FLAGS);
    if optimize && !sanitize {
        command.arg("/LTCG");
    }
    for p in &tool.libs {
        command.arg(format!("/LIBPATH:{}", p.display()));
    }
    command.arg("kernel32.lib");
    checked_command(&mut command)?;
    let backend_ms = start.elapsed().as_secs_f64() * 1000.0;
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "output has no parent"))?,
    )
    .map_err(io)?;
    let bytes = fs::read(exe).map_err(io)?;
    publish_library_artifact(&output, &bytes)?;
    let bytes = bytes.len() as u64;
    Ok(Build {
        output,
        bytes,
        backend_ms,
        runtime_rebuilt,
        sanitizer_runtime_dir,
        modules_reused: 0,
        modules_compiled: 1,
    })
}

/// Build the checked C library TU and its runtime into a real MSVC archive.
/// The receipt covers every published artifact, under the same output lock.
pub(crate) fn build_static_archive(
    c: &str,
    runtime_preamble: &str,
    header: &str,
    rust: &str,
    output: &Path,
    options: &BuildOptions,
) -> Result<(Build, bool)> {
    let providers = ffi_libraries(&options.ffi_libraries)?;
    if options.sanitize {
        return Err(Error::new(
            0,
            "static library export does not support AddressSanitizer",
        ));
    }
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("lib"))
    {
        return Err(Error::new(
            0,
            "static library output must have a .lib extension",
        ));
    }
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "output has no parent"))?,
    )
    .map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.with_extension("zn-lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let start = Instant::now();
    let tool = discover()?;
    let librarian = tool.compiler.with_file_name("lib.exe");
    let mut key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut key);
    hash_ffi(&providers, &mut key);
    "anode-static-library-c-abi-v1".hash(&mut key);
    c.hash(&mut key);
    header.hash(&mut key);
    rust.hash(&mut key);
    runtime_preamble.hash(&mut key);
    options.optimize.hash(&mut key);
    include_str!("runtime.c").hash(&mut key);
    include_str!("runtime.h").hash(&mut key);
    compiler_exe_digest()?.hash(&mut key);
    format!("{tool:?}").hash(&mut key);
    for path in [&tool.compiler, &librarian] {
        let metadata = fs::metadata(path).map_err(io)?;
        metadata.len().hash(&mut key);
        metadata.modified().map_err(io)?.hash(&mut key);
    }
    let object_key = key.finish();
    let identity = format!(
        "anode-lib-v1\n{object_key:016x}\nflags={}",
        if options.optimize {
            "/GL,/LTCG"
        } else {
            "ordinary"
        }
    );
    let receipt = output.with_extension("zn-lib-build");
    let header_path = output.with_extension("h");
    let rust_path = output.with_extension("rs");
    if !options.force {
        if let (Ok(bytes), Ok(actual_header), Ok(actual_rust)) = (
            fs::read(&output),
            fs::read_to_string(&header_path),
            fs::read_to_string(&rust_path),
        ) {
            let expected = format!("{identity}\n{:016x}\n", hash_bytes(&bytes));
            if actual_header == header
                && actual_rust == rust
                && fs::read_to_string(&receipt).is_ok_and(|s| s == expected)
            {
                if options.cache_bytes > 0 {
                    drop(crate::object_cache::Cache::open(
                        &cache_root().join("modules"),
                        options.cache_bytes,
                    )?);
                }
                return Ok((
                    Build {
                        output,
                        bytes: bytes.len() as u64,
                        backend_ms: 0.0,
                        runtime_rebuilt: false,
                        sanitizer_runtime_dir: None,
                        modules_reused: 0,
                        modules_compiled: 0,
                    },
                    true,
                ));
            }
        }
    }
    let scratch = Scratch::new()?;
    let provider_paths = stage_ffi(&providers, &scratch.path)?;
    let runtime_rebuilt = library_runtime(&tool, &scratch.path, runtime_preamble, options)?;
    let object = scratch.path.join("library.obj");
    let mut reused = false;
    if options.cache_bytes > 0 {
        let cache =
            crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
        reused = !options.force && cached_object(&cache, object_key, &object, options.optimize)?;
    }
    if !reused {
        fs::write(scratch.path.join("library.c"), c).map_err(io)?;
        checked_command(
            compiler(&tool, &scratch.path, options.optimize, false, true).args([
                "/c",
                "library.c",
                "/Folibrary.obj",
            ]),
        )?;
        if options.cache_bytes > 0 {
            let cache = crate::object_cache::Cache::open(
                &cache_root().join("modules"),
                options.cache_bytes,
            )?;
            cache.put(object_key, &object)?;
        }
    }
    let mut librarian = Command::new(librarian);
    librarian
        .current_dir(&scratch.path)
        .args(["/NOLOGO", "/OUT:library.lib", "library.obj", "runtime.obj"])
        .args(&provider_paths)
        .env_remove("LIB")
        .env_remove("LINK");
    if options.optimize {
        librarian.arg("/LTCG");
    }
    checked_command(&mut librarian)?;
    let bytes = fs::read(scratch.path.join("library.lib")).map_err(io)?;
    publish_library_artifact(&output, &bytes)?;
    publish_library_artifact(&header_path, header.as_bytes())?;
    publish_library_artifact(&rust_path, rust.as_bytes())?;
    publish_library_artifact(
        &receipt,
        format!("{identity}\n{:016x}\n", hash_bytes(&bytes)).as_bytes(),
    )?;
    Ok((
        Build {
            output,
            bytes: bytes.len() as u64,
            backend_ms: start.elapsed().as_secs_f64() * 1000.0,
            runtime_rebuilt,
            sanitizer_runtime_dir: None,
            modules_reused: usize::from(reused),
            modules_compiled: usize::from(!reused),
        },
        false,
    ))
}

fn library_runtime(
    tool: &Toolchain,
    scratch: &Path,
    preamble: &str,
    options: &BuildOptions,
) -> Result<bool> {
    let mut key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut key);
    "anode-library-runtime-v1".hash(&mut key);
    options.optimize.hash(&mut key);
    preamble.hash(&mut key);
    include_str!("runtime.c").hash(&mut key);
    include_str!("runtime.h").hash(&mut key);
    format!("{tool:?}").hash(&mut key);
    compiler_exe_digest()?.hash(&mut key);
    let metadata = fs::metadata(&tool.compiler).map_err(io)?;
    metadata.len().hash(&mut key);
    metadata.modified().map_err(io)?.hash(&mut key);
    let key = key.finish();
    let object = scratch.join("runtime.obj");
    if options.cache_bytes > 0 && !options.force {
        let cache =
            crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
        if cached_object(&cache, key, &object, options.optimize)? {
            return Ok(false);
        }
    }
    fs::write(scratch.join("runtime.h"), include_str!("runtime.h")).map_err(io)?;
    fs::write(
        scratch.join("runtime.c"),
        format!("{preamble}\n{}", include_str!("runtime.c")),
    )
    .map_err(io)?;
    checked_command(
        compiler(tool, scratch, true, false, options.optimize).args([
            "/c",
            "runtime.c",
            "/Foruntime.obj",
        ]),
    )?;
    if options.cache_bytes > 0 {
        let cache =
            crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
        cache.put(key, &object)?;
    }
    Ok(true)
}

/// Emit a reload DLL through the explicit scalar/POD C ABI. The internal
/// Cranelift aggregate calling convention is never exported as a C function.
pub(crate) fn build_reload_dll(
    prepared: &crate::lib_export::LibrarySource,
    output: &Path,
    options: &BuildOptions,
) -> Result<(Build, serde_json::Value)> {
    if options.sanitize {
        return Err(Error::new(0, "reload DLLs do not support AddressSanitizer"));
    }
    let providers = ffi_libraries(&options.ffi_libraries)?;
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "reload output has no parent"))?,
    )
    .map_err(io)?;
    let tool = discover()?;
    let start = Instant::now();
    let scratch = Scratch::new()?;
    let runtime_rebuilt =
        library_runtime(&tool, &scratch.path, &prepared.runtime_preamble, options)?;
    let providers_paths = stage_ffi(&providers, &scratch.path)?;
    fs::write(scratch.path.join("library.c"), &prepared.c).map_err(io)?;
    checked_command(
        compiler(&tool, &scratch.path, options.optimize, false, true).args([
            "/c",
            "library.c",
            "/Folibrary.obj",
        ]),
    )?;
    let mut exports = String::from("EXPORTS\n");
    for export in &prepared.exports {
        exports.push_str(&format!("{}\n", export.symbol));
    }
    fs::write(scratch.path.join("exports.def"), exports).map_err(io)?;
    let mut command = Command::new(tool.compiler.with_file_name("link.exe"));
    command
        .current_dir(&scratch.path)
        .args([
            "/NOLOGO",
            "/DLL",
            "library.obj",
            "runtime.obj",
            "/OUT:patch.dll",
            "/IMPLIB:patch-import.lib",
            "/DEF:exports.def",
        ])
        .args(LINK_FLAGS)
        .args(&providers_paths)
        .env_remove("LINK")
        .env_remove("LIB");
    if options.optimize {
        command.arg("/LTCG");
    }
    for path in &tool.libs {
        command.arg(format!("/LIBPATH:{}", path.display()));
    }
    command.arg("kernel32.lib");
    checked_command(&mut command)?;
    let dll = fs::read(scratch.path.join("patch.dll")).map_err(io)?;
    let object = fs::read(scratch.path.join("library.obj")).map_err(io)?;
    publish_library_artifact(&output, &dll)?;
    publish_library_artifact(&output.with_extension("obj"), &object)?;
    publish_library_artifact(&output.with_extension("h"), prepared.header.as_bytes())?;
    let providers = serde_json::Value::Array(
        providers
            .iter()
            .map(|p| serde_json::json!({"path":p.path,"sha256":p.sha256,"bytes":p.bytes.len()}))
            .collect(),
    );
    Ok((
        Build {
            output,
            bytes: dll.len() as u64,
            backend_ms: start.elapsed().as_secs_f64() * 1000.0,
            runtime_rebuilt,
            sanitizer_runtime_dir: None,
            modules_reused: 0,
            modules_compiled: 1,
        },
        providers,
    ))
}

fn build_modules(
    units: &[(String, String)],
    output: &Path,
    options: &BuildOptions,
    providers: &[FfiLibrary],
) -> Result<Build> {
    if units.is_empty() {
        return Err(Error::new(0, "no modules to build"));
    }
    let tool = discover()?;
    let scratch = Scratch::new()?;
    let providers = stage_ffi(providers, &scratch.path)?;
    let start = Instant::now();
    let runtime_rebuilt = runtime(
        &tool,
        &scratch.path,
        options.sanitize,
        options.optimize && !options.sanitize,
    )?;
    let cache =
        crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
    let mut tool_key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut tool_key);
    format!("{tool:?}").hash(&mut tool_key);
    compiler_exe_digest()?.hash(&mut tool_key);
    for path in [&tool.compiler, &tool.compiler.with_file_name("link.exe")] {
        let meta = fs::metadata(path).map_err(io)?;
        meta.len().hash(&mut tool_key);
        meta.modified().map_err(io)?.hash(&mut tool_key);
    }
    let tool_key = tool_key.finish();
    let mut objects = Vec::new();
    let mut missing = Vec::new();
    let mut reused = 0;
    for (i, (_, code)) in units.iter().enumerate() {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        tool_key.hash(&mut hash);
        code.hash(&mut hash);
        options.optimize.hash(&mut hash);
        options.sanitize.hash(&mut hash);
        let key = hash.finish();
        let object = format!("module{i}.obj");
        if !options.force
            && cached_object(
                &cache,
                key,
                &scratch.path.join(&object),
                options.optimize && !options.sanitize,
            )?
        {
            reused += 1;
        } else {
            let source = format!("module{i}.c");
            fs::write(scratch.path.join(&source), code).map_err(io)?;
            missing.push((key, source, object.clone()));
        }
        objects.push(object);
    }
    // Private copies are already in scratch; eviction by another build is safe.
    drop(cache);
    if !missing.is_empty() {
        let jobs = env::var("ZN_JOBS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(1, usize::from)
                    .min(4)
            })
            .clamp(1, 16);
        let mut command = compiler(
            &tool,
            &scratch.path,
            options.optimize,
            options.sanitize,
            true,
        );
        command
            .arg("/c")
            .arg(format!("/MP{}", jobs.min(missing.len())));
        for (_, source, _) in &missing {
            command.arg(source);
        }
        checked_command(&mut command)?;
        let cache =
            crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
        for (key, _, object) in &missing {
            cache.put(*key, &scratch.path.join(object))?;
        }
    }
    // Do not use the C compilation command here: /TC treats even .obj inputs
    // as C source. The driver infers object inputs and still configures ASan.
    let mut link = Command::new(&tool.compiler);
    link.current_dir(&scratch.path).args(["/nologo", "/MD"]);
    for key in ["CL", "_CL_", "LINK", "INCLUDE", "LIB"] {
        link.env_remove(key);
    }
    if options.sanitize {
        link.arg("/fsanitize=address");
    }
    link.args(&objects)
        .arg("/Feprogram.exe")
        .arg("/link")
        .arg("runtime.obj")
        .args(&providers)
        .args(LINK_FLAGS);
    if options.optimize && !options.sanitize {
        link.arg("/LTCG");
    }
    for path in &tool.libs {
        link.arg(format!("/LIBPATH:{}", path.display()));
    }
    link.arg("kernel32.lib");
    checked_command(&mut link)?;
    publish_library_artifact(
        output,
        &fs::read(scratch.path.join("program.exe")).map_err(io)?,
    )?;
    let sanitizer_runtime_dir = options
        .sanitize
        .then(|| tool.compiler.parent().unwrap().to_owned());
    Ok(Build {
        output: output.to_owned(),
        bytes: fs::metadata(output).map_err(io)?.len(),
        backend_ms: start.elapsed().as_secs_f64() * 1000.0,
        runtime_rebuilt,
        sanitizer_runtime_dir,
        modules_reused: reused,
        modules_compiled: units.len() - reused,
    })
}

pub fn build_fast(
    program: &crate::ir::CheckedProgram,
    output: &Path,
    options: &BuildOptions,
) -> Result<(Build, bool)> {
    let providers = ffi_libraries(&options.ffi_libraries)?;
    if options.sanitize {
        return Err(Error::new(
            0,
            "AddressSanitizer requires --backend c; no sanitizer checks are silently disabled",
        ));
    }
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(Error::new(0, "native output must have an .exe extension"));
    }
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "output has no parent"))?,
    )
    .map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.with_extension("zn-lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let start = Instant::now();
    let tool = discover()?;
    let mut key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut key);
    hash_ffi(&providers, &mut key);
    "anode-cranelift-v2".hash(&mut key);
    include_str!("runtime.c").hash(&mut key);
    include_str!("runtime.h").hash(&mut key);
    compiler_exe_digest()?.hash(&mut key);
    format!("{tool:?}").hash(&mut key);
    for p in [&tool.compiler, &tool.compiler.with_file_name("link.exe")] {
        let meta = fs::metadata(p).map_err(io)?;
        meta.len().hash(&mut key);
        meta.modified().map_err(io)?.hash(&mut key);
    }
    let environment_key = key.finish();
    let mut key = std::collections::hash_map::DefaultHasher::new();
    environment_key.hash(&mut key);
    program.hash(&mut key);
    (options.cache_bytes > 0).hash(&mut key);
    let object_key = key.finish();
    let identity = format!("anode-native-v1\n{object_key:016x}");
    let receipt = output.with_extension("zn-build");
    if !options.force {
        if let Ok(bytes) = fs::read(&output) {
            if fs::read_to_string(&receipt)
                .is_ok_and(|s| s == format!("{identity}\n{:016x}\n", hash_bytes(&bytes)))
            {
                if options.cache_bytes > 0 {
                    drop(crate::object_cache::Cache::open(
                        &cache_root().join("modules"),
                        options.cache_bytes,
                    )?);
                }
                return Ok((
                    Build {
                        output,
                        bytes: bytes.len() as u64,
                        backend_ms: 0.0,
                        runtime_rebuilt: false,
                        sanitizer_runtime_dir: None,
                        modules_reused: 0,
                        modules_compiled: 0,
                    },
                    true,
                ));
            }
        }
    }
    let scratch = Scratch::new()?;
    let provider_paths = stage_ffi(&providers, &scratch.path)?;
    let runtime_rebuilt = runtime(&tool, &scratch.path, false, false)?;
    let mut objects = Vec::new();
    let mut reused = 0;
    let mut compiled = 1;
    if options.cache_bytes == 0 {
        // Keep the small, whole-program path when shared caching is disabled.
        fs::write(
            scratch.path.join("program.obj"),
            crate::fast::object(program)?,
        )
        .map_err(io)?;
        objects.push("program.obj".to_string());
    } else {
        let analysis = crate::fast::analyze(program);
        let units = crate::native_units::plan(program, &analysis.hot);
        let cache =
            crate::object_cache::Cache::open(&cache_root().join("modules"), options.cache_bytes)?;
        let mut missing = Vec::new();
        let mut payloads: Vec<Option<Vec<u8>>> = vec![None; units.len()];
        for (index, unit) in units.iter().enumerate() {
            let mut key = std::collections::hash_map::DefaultHasher::new();
            environment_key.hash(&mut key);
            unit.fingerprint.hash(&mut key);
            let key = key.finish();
            let found = if options.force {
                None
            } else {
                cache
                    .get_bytes(key)?
                    .filter(|b| crate::coff_archive::object_payload(b, false))
            };
            if let Some(bytes) = found {
                payloads[index] = Some(bytes);
                reused += 1;
            } else {
                missing.push((index, unit, key));
            }
        }
        // Verified private byte snapshots survive concurrent eviction. Avoid
        // a scratch-file create/write per cache hit; bundle after generation.
        drop(cache);
        if !missing.is_empty() {
            let jobs = env::var("ZN_JOBS")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    std::thread::available_parallelism()
                        .map_or(1, usize::from)
                        .min(4)
                })
                .clamp(1, 16)
                .min(missing.len());
            let results = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for group in missing.chunks(missing.len().div_ceil(jobs)) {
                    let analysis = &analysis;
                    handles.push(scope.spawn(move || -> Result<Vec<(usize, Vec<u8>)>> {
                        let mut generated = Vec::new();
                        for (index, unit, _) in group {
                            let bytes =
                                crate::fast::object_unit(program, analysis, unit).map_err(|e| {
                                    Error::new(
                                        e.line,
                                        format!("native module {}: {}", unit.name, e.message),
                                    )
                                })?;
                            generated.push((*index, bytes));
                        }
                        Ok(generated)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(Error::new(0, "native module worker panicked")))
                    })
                    .collect::<Vec<_>>()
            });
            for result in results {
                for (index, bytes) in result? {
                    payloads[index] = Some(bytes);
                }
            }
            let cache = crate::object_cache::Cache::open(
                &cache_root().join("modules"),
                options.cache_bytes,
            )?;
            for (index, _, key) in &missing {
                cache.put_bytes(*key, payloads[*index].as_ref().unwrap())?;
            }
        }
        compiled = units.len() - reused;
        let entry = units
            .iter()
            .position(|u| u.functions.contains("main"))
            .ok_or_else(|| Error::new(0, "checked native program has no entry unit"))?;
        fs::write(
            scratch.path.join("program.obj"),
            payloads[entry].take().unwrap(),
        )
        .map_err(io)?;
        objects.push("program.obj".into());
        let mut members = Vec::new();
        for (i, unit) in units.iter().enumerate() {
            if i == entry {
                continue;
            }
            let mut symbols = unit
                .functions
                .iter()
                .filter(|name| {
                    program
                        .functions
                        .iter()
                        .any(|f| f.source.name == **name && f.source.external_symbol.is_none())
                })
                .map(|name| format!("zn_fn_{name}"))
                .collect::<Vec<_>>();
            symbols.extend(
                unit.functions
                    .intersection(&analysis.workers)
                    .map(|name| format!("zn_worker_{name}")),
            );
            members.push(crate::coff_archive::Member {
                bytes: payloads[i].take().unwrap(),
                symbols,
            });
        }
        if !members.is_empty() {
            fs::write(
                scratch.path.join("modules.lib"),
                crate::coff_archive::build(&members)?,
            )
            .map_err(io)?;
            objects.push("modules.lib".into());
        }
    }
    let mut link = Command::new(tool.compiler.with_file_name("link.exe"));
    link.current_dir(&scratch.path)
        .arg("/NOLOGO")
        .args(&objects)
        .args(&provider_paths)
        .args(["runtime.obj", "/OUT:program.exe", "/SUBSYSTEM:CONSOLE"])
        .args(LINK_FLAGS);
    for p in &tool.libs {
        link.arg(format!("/LIBPATH:{}", p.display()));
    }
    link.arg("kernel32.lib")
        .env_remove("LINK")
        .env_remove("LIB");
    checked_command(&mut link)?;
    let bytes = fs::read(scratch.path.join("program.exe")).map_err(io)?;
    publish_library_artifact(&output, &bytes)?;
    publish_library_artifact(
        &receipt,
        format!("{identity}\n{:016x}\n", hash_bytes(&bytes)).as_bytes(),
    )?;
    Ok((
        Build {
            output,
            bytes: bytes.len() as u64,
            backend_ms: start.elapsed().as_secs_f64() * 1000.0,
            runtime_rebuilt,
            sanitizer_runtime_dir: None,
            modules_reused: reused,
            modules_compiled: compiled,
        },
        false,
    ))
}

/// Identity separates optimization tiers and includes checked field contracts,
/// the compiler implementation, runtime and installed native toolchain.
fn baked_key(program: &crate::ir::CheckedProgram) -> Result<u64> {
    if program
        .functions
        .iter()
        .any(|f| f.source.external_symbol.is_some())
    {
        return Err(Error::new(0, "baked artifacts with outbound FFI require an explicit provider identity set; use the normal build path with --link-lib"));
    }
    let tool = discover()?;
    let mut key = std::collections::hash_map::DefaultHasher::new();
    hash_codegen_flags(&mut key);
    "anode-baked-c-o2-v1".hash(&mut key);
    program.hash(&mut key);
    include_str!("runtime.c").hash(&mut key);
    include_str!("runtime.h").hash(&mut key);
    compiler_exe_digest()?.hash(&mut key);
    format!("{tool:?}").hash(&mut key);
    for p in [&tool.compiler, &tool.compiler.with_file_name("link.exe")] {
        let meta = fs::metadata(p).map_err(io)?;
        meta.len().hash(&mut key);
        meta.modified().map_err(io)?.hash(&mut key);
    }
    Ok(key.finish())
}

/// Bake an immutable, already checked IR snapshot into the bounded shared store.
/// No project executable is replaced. Scratch C/objects/executable are removed.
pub fn bake(program: &crate::ir::CheckedProgram, budget: u64, force: bool) -> Result<(u64, bool)> {
    if budget == 0 {
        return Err(Error::new(
            0,
            "baking needs cache space: set cache_mb or --cache-mb (for example 64)",
        ));
    }
    let key = baked_key(program)?;
    let scratch = Scratch::new()?;
    let exe = scratch.path.join("baked.exe");
    {
        let cache = crate::object_cache::Cache::open(&cache_root().join("modules"), budget)?;
        if !force && cache.get(key, &exe)? {
            return Ok((key, true));
        }
    }
    let c = crate::cgen::generate(program, true).flatten();
    build_checked(&c, &exe, true, false)?;
    let cache = crate::object_cache::Cache::open(&cache_root().join("modules"), budget)?;
    if !cache.put(key, &exe)? {
        return Err(Error::new(
            0,
            "optimized executable exceeds cache budget; increase cache_mb",
        ));
    }
    Ok((key, false))
}

/// Exact checked-input match only. A stale or evicted bake is an ordinary miss.
pub fn restore_baked(
    program: &crate::ir::CheckedProgram,
    output: &Path,
    budget: u64,
) -> Result<Option<Build>> {
    if budget == 0 {
        return Ok(None);
    }
    if output
        .extension()
        .is_none_or(|e| !e.eq_ignore_ascii_case("exe"))
    {
        return Err(Error::new(0, "native output must have an .exe extension"));
    }
    let output = if output.is_absolute() {
        output.to_owned()
    } else {
        env::current_dir().map_err(io)?.join(output)
    };
    fs::create_dir_all(
        output
            .parent()
            .ok_or_else(|| Error::new(0, "output has no parent"))?,
    )
    .map_err(io)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.with_extension("zn-lock"))
        .map_err(io)?;
    lock.lock().map_err(io)?;
    let start = Instant::now();
    let key = baked_key(program)?;
    let scratch = Scratch::new()?;
    let tmp = scratch.path.join("baked.exe");
    let cache = crate::object_cache::Cache::open(&cache_root().join("modules"), budget)?;
    let hit = cache.get(key, &tmp)?;
    drop(cache);
    if !hit {
        return Ok(None);
    }
    let bytes = fs::read(&tmp).map_err(io)?;
    // An unchanged running binary does not need to be replaced on Windows.
    if !fs::read(&output).is_ok_and(|old| old == bytes) {
        publish_library_artifact(&output, &bytes)?;
    }
    publish_library_artifact(
        &output.with_extension("zn-build"),
        format!(
            "anode-baked-v1\n{key:016x}\n{:016x}\nflags=/GL,/LTCG\n",
            hash_bytes(&bytes)
        )
        .as_bytes(),
    )?;
    Ok(Some(Build {
        output,
        bytes: bytes.len() as u64,
        backend_ms: start.elapsed().as_secs_f64() * 1000.0,
        runtime_rebuilt: false,
        sanitizer_runtime_dir: None,
        modules_reused: 1,
        modules_compiled: 0,
    }))
}
