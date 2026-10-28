//! Explicit one-shot background optimization. No daemon, hot-swap or model calls.
use crate::{backend, project::Project, storage, Error, Result};
use std::{
    env, fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};
fn io(e: std::io::Error) -> Error {
    Error::new(0, e.to_string())
}

pub fn run(
    input: &Path,
    budget: Option<u64>,
    background: bool,
    worker: bool,
    force: bool,
) -> Result<()> {
    let layout = Project::layout(input)?;
    let budget = budget.unwrap_or(layout.cache_bytes);
    if budget == 0 {
        return Err(Error::new(
            0,
            "baking needs cache space: set cache_mb or --cache-mb 64",
        ));
    }
    if background {
        if !layout.managed {
            return Err(Error::new(0,"background bake requires a Zenith.toml project; foreground bake also accepts single files"));
        }
        storage::prepare(&layout)?;
        let mut c = Command::new(env::current_exe().map_err(io)?);
        c.arg("bake")
            .arg(input.canonicalize().map_err(io)?)
            .arg("--worker")
            .arg("--cache-mb")
            .arg((budget / 1024 / 1024).to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if force {
            c.arg("--rebuild");
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS. Children inherit priority.
            c.creation_flags(0x08000000 | 0x00004000);
        }
        let child = c.spawn().map_err(io)?;
        println!(
            "Background bake launched (PID {}). Status: {}",
            child.id(),
            layout.root.join(".zenith/build/bake-status.txt").display()
        );
        return Ok(());
    }
    // One optimizer per project. The lock is released by the OS even on a crash.
    let mut status = None;
    let _lock = if layout.managed {
        storage::prepare(&layout)?;
        let dir = layout.root.join(".zenith/build");
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join("bake.lock"))
            .map_err(io)?;
        lock.try_lock()
            .map_err(|_| Error::new(0, "a bake is already running for this project"))?;
        status = Some(fs::File::create(dir.join("bake-status.txt")).map_err(io)?);
        Some(lock)
    } else {
        None
    };
    if let Some(log) = &mut status {
        writeln!(log, "RUNNING pid={}", std::process::id()).map_err(io)?;
    }
    let start = Instant::now();
    let result = (|| {
        // Load once: ongoing editor changes cannot mutate this checked IR snapshot.
        let project = Project::load(input)?;
        let program = project.checked(false)?.0;
        let (key, reused) = backend::bake(&program, budget, force)?;
        Ok(format!("READY key={key:016x} reused={reused} elapsed_ms={:.3}\nUse zn build --baked (or zn run --baked). Source is checked again; only an exact match can use this artifact.",start.elapsed().as_secs_f64()*1000.0))
    })();
    match result {
        Ok(message) => {
            if let Some(log) = &mut status {
                writeln!(log, "{message}").map_err(io)?;
            }
            if !worker {
                println!("{message}");
            }
            Ok(())
        }
        Err(error) => {
            if let Some(log) = &mut status {
                let _ = writeln!(log, "FAILED {error}");
            }
            Err(error)
        }
    }
}
