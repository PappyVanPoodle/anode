use std::{
    collections::VecDeque,
    env, fs,
    io::{Read, Write},
    path::PathBuf,
    process::ExitCode,
    time::Instant,
};
use zenith_language::{
    backend, baking, lib_export, model, object_cache,
    project::{self, Project},
    storage, Error, Result,
};

fn run() -> Result<u8> {
    let mut args: VecDeque<String> = env::args().skip(1).collect();
    let mut before_program_args = true;
    let mut json_output = false;
    args.retain(|s| {
        if s == "--" {
            before_program_args = false;
        }
        if before_program_args && s == "--json" {
            json_output = true;
            false
        } else {
            true
        }
    });
    let command = args.pop_front().unwrap_or_else(|| "help".into());
    if command == "verify" {
        return verify_command(args, json_output);
    }
    if command == "hot-reload" {
        return hot_reload_command(args, json_output);
    }
    if command == "fmt" {
        return format_command(args, json_output);
    }
    if command == "agent" {
        let path = PathBuf::from(args.pop_front().unwrap_or_else(|| ".".into()));
        if !args.is_empty() {
            return Err(Error::new(0, "agent accepts one project path"));
        }
        return zenith_language::agent::serve(&path).map(|()| 0);
    }
    if command == "context" {
        let name = args
            .pop_front()
            .ok_or_else(|| Error::new(0, "context requires a module-qualified function name"))?;
        let path = PathBuf::from(args.pop_front().unwrap_or_else(|| ".".into()));
        if !args.is_empty() {
            return Err(Error::new(0, "context accepts FUNCTION [PROJECT]"));
        }
        println!(
            "{}",
            zenith_language::agent::context(&Project::load(&path)?, &name)?
        );
        return Ok(0);
    }
    if ["help", "--help", "-h"].contains(&command.as_str()) {
        println!("Anode {} — Rust-written compiler, native Windows x64\n\nanode new DIRECTORY\nanode check [PROJECT|FILE.an] [--json]\nanode fmt [PROJECT|FILE.an] [--check] [--json]\nanode verify [PROJECT|FILE.an] [--plan] [--function NAME] [--emit-test] [--max-cases N] [--max-steps N] [--timeout-ms N] [--json]\nanode verify [PROJECT|FILE.an] --batch REQUEST.json [--json]\nanode verify [PROJECT|FILE.an] --campaign [REQUEST.json] [--max-workers N] [--json]\nanode hot-reload BASELINE UPDATED [-o PATCH.dll] [--migrate-fn NAME] [--release] [--rebuild] [--link-lib FILE.lib] [--cache-mb N] [--json]\nanode build [PROJECT|FILE.an] --lib [-o NAME.lib] [--release]\nanode build [PROJECT|FILE.an] [-o FILE.exe] [--backend fast|c] [--release] [--sanitize] [--rebuild] [--link-lib FILE.lib]\nanode run [PROJECT|FILE.an] [OPTIONS] -- [PROGRAM ARGUMENTS]\nanode test [PROJECT|FILE.an] [--jobs N] [--release] [--sanitize]\nanode bake [PROJECT|FILE.an] [--background] [--cache-mb N]\nanode build/run [PROJECT|FILE.an] --baked [--cache-mb N]\nanode storage [PROJECT]\nanode clean [PROJECT] [--cache]\nanode emit-c [PROJECT|FILE.an] -o FILE.c\nanode emit-ir [PROJECT|FILE.an] [-o FILE.txt]\nanode review-prompt [PROJECT|FILE.an]\nanode review [PROJECT|FILE.an] --model INSTALLED_MODEL [--server 127.0.0.1:11434]\nanode doctor [--refresh]\n\nOmit PROJECT inside a directory containing Anode.toml. Legacy zn/.zn/Zenith.toml projects remain supported.\nanode context FUNCTION [PROJECT]\nanode agent [PROJECT]   (JSON Lines on stdin/stdout)\nModel review is optional and advisory.", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if command == "--version" || command == "version" {
        println!("Anode {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if command == "new" {
        let path = args
            .pop_front()
            .ok_or_else(|| Error::new(0, "new requires a new directory name"))?;
        if !args.is_empty() {
            return Err(Error::new(0, "new accepts one directory"));
        }
        project::create(&PathBuf::from(&path))?;
        println!("Created {path}\nNext: anode run {path}\nTests: anode test {path}");
        return Ok(0);
    }
    if command == "doctor" {
        for arg in args {
            if arg == "--refresh" {
                backend::refresh_toolchain()?;
            } else {
                return Err(Error::new(0, "doctor accepts only --refresh"));
            }
        }
        let t = backend::discover()?;
        println!("MSVC: {}\nNative backend: Windows x64\nCompiler: Rust + Cranelift (fast); MSVC (release/ASan)\nModel: optional loopback Ollama API; never auto-installed",t.compiler.display());
        return Ok(0);
    }
    if ![
        "check",
        "build",
        "bake",
        "run",
        "test",
        "emit-c",
        "emit-ir",
        "review",
        "review-prompt",
        "storage",
        "clean",
    ]
    .contains(&command.as_str())
    {
        return Err(Error::new(0, "unknown command; use zn --help"));
    }
    let input = PathBuf::from(if args.front().is_some_and(|a| !a.starts_with('-')) {
        args.pop_front().unwrap()
    } else {
        ".".into()
    });
    let mut output = None;
    let mut options = backend::BuildOptions::default();
    let mut model_name = None;
    let mut address = "127.0.0.1:11434".to_string();
    let mut program_args = Vec::new();
    let mut clear_cache = false;
    let mut backend_name = "fast".to_string();
    let mut explicit_backend = false;
    let mut cache_override = None;
    let mut baked = false;
    let mut background = false;
    let mut worker = false;
    let mut library = false;
    let mut test_jobs = None;
    while let Some(arg) = args.pop_front() {
        match arg.as_str() {
            "--jobs" if command == "test" => {
                if test_jobs.is_some() {
                    return Err(Error::new(0, "provide only one --jobs value"));
                }
                let jobs = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, "--jobs requires an integer from 1 to 16"))?
                    .parse::<usize>()
                    .map_err(|_| Error::new(0, "test jobs must be from 1 to 16"))?;
                if !(1..=16).contains(&jobs) {
                    return Err(Error::new(0, "test jobs must be from 1 to 16"));
                }
                test_jobs = Some(jobs);
            }
            "--lib" if command == "build" => library = true,
            "--link-lib" if ["build", "run", "test"].contains(&command.as_str()) => {
                options
                    .ffi_libraries
                    .push(PathBuf::from(args.pop_front().ok_or_else(|| {
                        Error::new(0, "--link-lib requires FILE.lib")
                    })?));
            }
            "--cache-mb" => {
                let n: u64 = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, "missing cache budget"))?
                    .parse()
                    .map_err(|_| Error::new(0, "cache budget must be 0..4096 MiB"))?;
                if n > 4096 {
                    return Err(Error::new(0, "cache budget must be 0..4096 MiB"));
                }
                cache_override = Some(n * 1024 * 1024);
            }
            "--baked" if command == "build" || command == "run" => baked = true,
            "--background" if command == "bake" => background = true,
            "--worker" if command == "bake" => worker = true,
            "--backend" => {
                backend_name = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, "missing backend: fast or c"))?;
                if !["fast", "c"].contains(&backend_name.as_str()) {
                    return Err(Error::new(0, "backend must be fast or c"));
                }
                explicit_backend = true;
            }
            "-o" => {
                output = Some(PathBuf::from(
                    args.pop_front()
                        .ok_or_else(|| Error::new(0, "missing output path"))?,
                ))
            }
            "--release" => options.optimize = true,
            "--sanitize" => options.sanitize = true,
            "--rebuild" => options.force = true,
            "--cache" if command == "clean" => clear_cache = true,
            "--model" => {
                model_name = Some(
                    args.pop_front()
                        .ok_or_else(|| Error::new(0, "missing model name"))?,
                )
            }
            "--server" => {
                address = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, "missing server"))?
            }
            "--" if command == "run" => {
                program_args.extend(args);
                break;
            }
            _ => return Err(Error::new(0, format!("unknown option {arg}"))),
        }
    }
    if command == "bake" {
        if output.is_some()
            || explicit_backend
            || options.sanitize
            || options.optimize
            || (background && worker)
        {
            return Err(Error::new(0,"bake accepts only --background, --rebuild and --cache-mb; it stores optimized output without replacing an EXE"));
        }
        return baking::run(&input, cache_override, background, worker, options.force).map(|()| 0);
    }
    if baked
        && (explicit_backend
            || options.sanitize
            || options.optimize
            || !options.ffi_libraries.is_empty())
    {
        return Err(Error::new(0,"--baked selects a cached optimized build or the fast fallback; do not combine it with backend/release/sanitize"));
    }
    if command == "storage" || command == "clean" {
        let project = Project::layout(&input)?;
        if command == "storage" {
            println!("{}", storage::report(&project)?);
        } else {
            let (bytes, files) = storage::clean(&project)?;
            println!("Removed {files} generated files, {bytes} bytes. Source files preserved.");
            if clear_cache {
                let cache = backend::cache_root().join("modules");
                if cache.exists() {
                    let _cache = object_cache::Cache::open(&cache, 0)?;
                }
                println!("Shared module objects cleared; small runtime cache retained.");
            }
        }
        return Ok(0);
    }
    let start = Instant::now();
    let project = Project::load(&input)?;
    options.cache_bytes = cache_override.unwrap_or(project.cache_bytes);
    if library {
        if options.sanitize || baked || (explicit_backend && backend_name != "c") {
            return Err(Error::new(
                0,
                "--lib uses the C ABI backend; do not combine it with --sanitize, --baked or --backend fast",
            ));
        }
        let output = output.unwrap_or_else(|| {
            project
                .output(false, options.optimize, false)
                .with_extension("lib")
        });
        for path in [
            &output,
            &output.with_extension("h"),
            &output.with_extension("rs"),
        ] {
            protect_sources(&project, path)?;
        }
        if project.managed {
            storage::prepare(&project)?;
        }
        let built = lib_export::build_static_lib(
            &project,
            &output,
            &lib_export::LibraryOptions {
                optimize: options.optimize,
                force: options.force,
                cache_bytes: options.cache_bytes,
                ffi_libraries: options.ffi_libraries,
            },
        )?;
        eprintln!(
            "{}",
            serde_json::json!({
                "tier": "static-library",
                "output": built.output,
                "header": built.header,
                "rust": built.rust,
                "bytes": built.bytes,
                "exports": built.exports.len(),
                "reused": built.reused,
                "runtime_rebuilt": built.runtime_rebuilt,
                "backend_ms": built.backend_ms,
                "total_ms": start.elapsed().as_secs_f64() * 1000.0,
            })
        );
        return Ok(0);
    }
    let testing = command == "test";
    if !explicit_backend && (options.sanitize || options.optimize) {
        backend_name = "c".into();
    }
    let use_fast = backend_name == "fast" && command != "emit-c";
    if options.cache_bytes > 0
        && (!use_fast || !["build", "run", "test"].contains(&command.as_str()))
    {
        // Native builders enforce this cap themselves, including EXE hits.
        // Other commands/C receipts still need their normal maintenance pass.
        drop(object_cache::Cache::open(
            &backend::cache_root().join("modules"),
            options.cache_bytes,
        )?);
    }
    if testing {
        let output =
            output.unwrap_or_else(|| project.output(true, options.optimize, options.sanitize));
        protect_sources(&project, &output)?;
        if project.managed {
            storage::prepare(&project)?;
        }
        let jobs = test_jobs.unwrap_or(1);
        let report = backend::run_tests_with_jobs(&project, &output, &options, use_fast, jobs)?;
        let build = &report.build;
        eprintln!(
            "{}",
            serde_json::json!({
                "tier": if use_fast { "fast" } else { "c" },
                "output": build.output,
                "bytes": build.bytes,
                "reused": report.reused,
                "modules_reused": build.modules_reused,
                "modules_compiled": build.modules_compiled,
                "runtime_rebuilt": build.runtime_rebuilt,
                "backend_ms": build.backend_ms,
                "total_ms": start.elapsed().as_secs_f64() * 1000.0,
                "test_jobs": jobs,
                "test_cases": report.cases.len(),
            })
        );
        std::io::stdout()
            .lock()
            .write_all(&report.stdout)
            .map_err(|e| Error::new(0, e.to_string()))?;
        std::io::stderr()
            .lock()
            .write_all(&report.stderr)
            .map_err(|e| Error::new(0, e.to_string()))?;
        return Ok(u8::from(!report.success()));
    }
    let (checked, c, units) = if use_fast || command == "emit-ir" {
        (Some(project.checked(testing)?.0), String::new(), Vec::new())
    } else {
        let artifact = project.build_input(testing, options.cache_bytes > 0)?;
        (None, artifact.c, artifact.units)
    };
    let check_ms = start.elapsed().as_secs_f64() * 1000.0;
    match command.as_str() {
        "emit-ir" => {
            let text = checked.as_ref().unwrap().dump();
            if let Some(path) = output {
                protect_sources(&project, &path)?;
                write_generated_source(&path, text.as_bytes())?;
            } else {
                print!("{text}");
            }
        }
        "check" => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"ok":true,"checked_files":project.files.len(),"elapsed_ms":check_ms})
                );
            } else {
                println!(
                    "Checked {} source files in {check_ms:.3} ms",
                    project.files.len()
                );
            }
        }
        "review-prompt" => println!("{}", model::project_prompt(&project)?),
        "review" => {
            let name = model_name.ok_or_else(|| {
                Error::new(0, "provide --model for an already installed local model")
            })?;
            println!(
                "ADVISORY MODEL RESPONSE — unverified; compiler checks are unchanged\n{}",
                model::review_project(&project, &name, &address)?
            );
        }
        "emit-c" => {
            let output = output.ok_or_else(|| Error::new(0, "emit-c requires -o FILE.c"))?;
            if output.extension().is_none_or(|e| e != "c") {
                return Err(Error::new(0, "emit-c output must end in .c"));
            }
            protect_sources(&project, &output)?;
            write_generated_source(&output, c.as_bytes())?;
        }
        _ => {
            let output = output
                .unwrap_or_else(|| project.output(testing, options.optimize, options.sanitize));
            protect_sources(&project, &output)?;
            if project.managed {
                storage::prepare(&project)?;
            }
            let optimized = if baked && !options.force {
                backend::restore_baked(checked.as_ref().unwrap(), &output, options.cache_bytes)?
            } else {
                None
            };
            let tier = if optimized.is_some() {
                "baked"
            } else if use_fast {
                "fast"
            } else {
                "c"
            };
            let (build, reused) = if let Some(build) = optimized {
                (build, true)
            } else if use_fast {
                backend::build_fast(checked.as_ref().unwrap(), &output, &options)?
            } else {
                backend::build_reusing(&c, &units, &output, &options)?
            };

            eprintln!("{{\"tier\":\"{tier}\",\"output\":{},\"bytes\":{},\"reused\":{},\"modules_reused\":{},\"modules_compiled\":{},\"runtime_rebuilt\":{},\"frontend_ms\":{check_ms:.3},\"backend_ms\":{:.3},\"total_ms\":{:.3}}}",model::json_string(&build.output.to_string_lossy()),build.bytes,reused,build.modules_reused,build.modules_compiled,build.runtime_rebuilt,build.backend_ms,start.elapsed().as_secs_f64()*1000.0);
            if command == "run" {
                let status = build
                    .command()?
                    .args(program_args)
                    .status()
                    .map_err(|e| Error::new(0, e.to_string()))?;
                return Ok(status
                    .code()
                    .and_then(|code| u8::try_from(code).ok())
                    .unwrap_or(1));
            }
        }
    }
    Ok(0)
}
fn verify_command(mut args: VecDeque<String>, json_output: bool) -> Result<u8> {
    let mut input = None;
    let mut batch = None;
    let mut campaign = false;
    let mut campaign_request = None;
    let mut max_workers = None;
    let mut campaign_options = serde_json::Map::new();
    let mut contract_options = false;
    let mut options = zenith_language::verify::VerificationOptions {
        execute: true,
        ..Default::default()
    };
    while let Some(arg) = args.pop_front() {
        match arg.as_str() {
            "--campaign" => {
                if campaign {
                    return Err(Error::new(0, "provide only one --campaign request"));
                }
                campaign = true;
                if args.front().is_some_and(|arg| !arg.starts_with('-')) {
                    campaign_request = args.pop_front().map(PathBuf::from);
                }
            }
            "--max-workers" => {
                if max_workers.is_some() {
                    return Err(Error::new(0, "provide only one --max-workers"));
                }
                let count = args
                    .pop_front()
                    .and_then(|s| s.parse::<u64>().ok())
                    .filter(|n| (1..=16).contains(n))
                    .ok_or_else(|| {
                        Error::new(0, "--max-workers requires an integer from 1 to 16")
                    })?;
                max_workers = Some(count);
            }
            "--batch" => {
                if batch.is_some() {
                    return Err(Error::new(0, "provide only one --batch request"));
                }
                batch =
                    Some(PathBuf::from(args.pop_front().ok_or_else(|| {
                        Error::new(0, "--batch requires REQUEST.json")
                    })?));
            }
            "--execute" | "--plan" => {
                contract_options = true;
                options.execute = arg == "--execute";
                campaign_options.insert("execute".to_owned(), serde_json::json!(options.execute));
            }
            "--emit-test" => {
                contract_options = true;
                options.emit_test = true;
                campaign_options.insert("emit_test".to_owned(), serde_json::json!(true));
            }
            "--function" => {
                contract_options = true;
                options.functions.push(
                    args.pop_front()
                        .ok_or_else(|| Error::new(0, "--function requires a name"))?,
                );
            }
            "--max-cases" | "--max-steps" | "--timeout-ms" => {
                contract_options = true;
                let value = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, format!("{arg} requires a positive integer")))?
                    .parse::<u64>()
                    .map_err(|_| Error::new(0, format!("{arg} requires a positive integer")))?;
                campaign_options.insert(
                    arg.trim_start_matches("--").replace('-', "_"),
                    serde_json::json!(value),
                );
                match arg.as_str() {
                    "--max-cases" => {
                        options.max_cases = usize::try_from(value)
                            .map_err(|_| Error::new(0, "case limit exceeds platform size"))?
                    }
                    "--max-steps" => {
                        options.max_steps = usize::try_from(value)
                            .map_err(|_| Error::new(0, "step limit exceeds platform size"))?
                    }
                    _ => options.timeout_ms = value,
                }
            }
            _ if !arg.starts_with('-') => {
                if input.replace(PathBuf::from(arg)).is_some() {
                    return Err(Error::new(0, "verify accepts one project or source path"));
                }
            }
            _ => return Err(Error::new(0, format!("unknown verify option {arg}"))),
        }
    }
    if campaign && batch.is_some() {
        return Err(Error::new(
            0,
            "--campaign and --batch are mutually exclusive",
        ));
    }
    if max_workers.is_some() && !campaign {
        return Err(Error::new(0, "--max-workers requires --campaign planning"));
    }
    if campaign && !options.functions.is_empty() {
        return Err(Error::new(
            0,
            "--function cannot restrict campaign verification; every completed function is checked",
        ));
    }
    if batch.is_some() && contract_options {
        return Err(Error::new(0,"--batch uses its JSON verify_contracts options; omit --execute/--plan/--function/--emit-test and contract budget flags"));
    }
    let project = Project::load(&input.unwrap_or_else(|| PathBuf::from(".")))?;
    if let Some(request) = batch {
        return verify_batch_command(&project, &request, json_output);
    }
    if campaign {
        return verify_campaign_command(
            &project,
            campaign_request.as_deref(),
            max_workers,
            campaign_options,
            json_output,
        );
    }
    let report = zenith_language::verify::verify_contracts(&project, &options)?;
    if json_output {
        println!("{report}");
    } else {
        println!(
            "Contract boundary check: {}",
            report["status"].as_str().unwrap_or("incomplete")
        );
        if !report["totals"].is_null() {
            println!("{}", report["totals"]);
        }
        if let Some(diagnostics) = report["diagnostics"].as_array() {
            for diagnostic in diagnostics {
                println!(
                    "{}:{}: {}",
                    diagnostic["file"].as_str().unwrap_or("source"),
                    diagnostic["line"].as_u64().unwrap_or(0),
                    diagnostic["message"].as_str().unwrap_or("invalid source")
                );
            }
        }
        if let Some(functions) = report["functions"].as_array() {
            for function in functions {
                println!(
                    "{}: {}",
                    function["function"].as_str().unwrap_or("?"),
                    function["status"].as_str().unwrap_or("incomplete")
                );
                if let Some(reason) = function["reason"].as_str() {
                    println!("  {reason}");
                }
                if let Some(cases) = function["cases"].as_array() {
                    for case in cases.iter().filter(|case| {
                        matches!(
                            case["status"].as_str(),
                            Some("counterexample" | "runtime_fault" | "budget_exhausted")
                        )
                    }) {
                        println!("  {case}");
                    }
                }
            }
        }
        if let Some(candidates) = report["test_candidates"].as_array() {
            for candidate in candidates {
                println!(
                    "Test proposal: {}\nExpected outcome: {}\nChecked: {}",
                    candidate["file"].as_str().unwrap_or("source"),
                    candidate["expected_outcome"]
                        .as_str()
                        .unwrap_or("unavailable"),
                    candidate["checked"] == true,
                );
                if let Some(source) = candidate["source"].as_str() {
                    println!("{source}");
                }
            }
        }
        println!("Finite boundary evidence; full compiler checks remain authoritative.");
    }
    Ok(match report["status"].as_str() {
        Some("failed" | "invalid") => 1,
        Some("incomplete") => 2,
        _ if options.execute && report["totals"]["executed_cases"].as_u64().unwrap_or(0) == 0 => 2,
        _ => 0,
    })
}

fn verify_campaign_command(
    project: &Project,
    request_path: Option<&std::path::Path>,
    max_workers: Option<u64>,
    cli_options: serde_json::Map<String, serde_json::Value>,
    json_output: bool,
) -> Result<u8> {
    let mut request = if let Some(path) = request_path {
        const MAX_REQUEST: u64 = 16 * 1024 * 1024;
        let mut bytes = Vec::new();
        fs::File::open(path)
            .map_err(|e| Error::new(0, e.to_string()).in_file(path))?
            .take(MAX_REQUEST + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::new(0, e.to_string()).in_file(path))?;
        if bytes.len() as u64 > MAX_REQUEST {
            return Err(Error::new(0, "campaign request exceeds 16 MiB").in_file(path));
        }
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|e| Error::new(0, format!("invalid campaign JSON: {e}")).in_file(path))?
    } else {
        serde_json::json!({})
    };
    let object = request
        .as_object_mut()
        .ok_or_else(|| Error::new(0, "campaign request must be a JSON object"))?;
    // This CLI verifies the disk project. Editor overlays use the versioned
    // agent/LSP endpoints instead of silently verifying a different snapshot.
    if object.contains_key("overlays") || object.contains_key("patches") {
        return Err(Error::new(
            0,
            "CLI campaigns use disk sources; use agent/LSP for overlays or patches",
        ));
    }
    let verifying = object.contains_key("responses");
    let action = if verifying { "verify" } else { "plan" };
    let method = if verifying {
        "verify_campaign"
    } else {
        "plan_campaign"
    };
    if object
        .get("action")
        .is_some_and(|v| v.as_str() != Some(action))
        || object
            .get("method")
            .is_some_and(|v| !matches!(v.as_str(), Some("campaign")) && v.as_str() != Some(method))
        || object
            .get("command")
            .is_some_and(|v| v.as_str() != Some("campaign"))
    {
        return Err(Error::new(
            0,
            "campaign action/method must agree with the presence of responses",
        ));
    }
    if verifying {
        if max_workers.is_some() {
            return Err(Error::new(
                0,
                "--max-workers is a planning option; verification uses the pinned assignments",
            ));
        }
        if !cli_options.is_empty() {
            let options = object
                .entry("verify_contracts")
                .or_insert_with(|| serde_json::json!({}));
            let options = options
                .as_object_mut()
                .ok_or_else(|| Error::new(0, "verify_contracts must be an options object"))?;
            options.extend(cli_options);
        }
    } else {
        if !cli_options.is_empty() {
            return Err(Error::new(
                0,
                "contract flags require a campaign response request; planning never executes",
            ));
        }
        if object.contains_key("campaign")
            || object.contains_key("verify_contracts")
            || object.contains_key("apply")
        {
            return Err(Error::new(
                0,
                "campaign verification requires responses; planning cannot apply edits",
            ));
        }
        if let Some(count) = max_workers {
            if object
                .get("max_workers")
                .is_some_and(|value| value.as_u64() != Some(count))
            {
                return Err(Error::new(0, "conflicting JSON and CLI max_workers"));
            }
            object.insert("max_workers".to_owned(), serde_json::json!(count));
        }
    }
    let report = if verifying {
        zenith_language::holes::verify_campaign(project, &request)?
    } else {
        zenith_language::holes::plan_campaign(project, &request)?
    };
    if json_output {
        println!("{report}");
    } else {
        println!(
            "Campaign: {}\nSource files written: {}",
            report["status"].as_str().unwrap_or("failed"),
            report["writes"].as_u64().unwrap_or(0)
        );
        if !verifying {
            for worker in report["workers"].as_array().into_iter().flatten() {
                println!(
                    "{}: {} holes\n{}",
                    worker["worker_id"].as_str().unwrap_or("?"),
                    worker["hole_ids"].as_array().map_or(0, Vec::len),
                    worker["brief"]
                );
            }
        } else {
            println!(
                "Composition: {}\nMissing holes: {}",
                report["composed"]["status"].as_str().unwrap_or("invalid"),
                report["missing_holes"].as_array().map_or(0, Vec::len)
            );
            if !report["contract_evidence"].is_null() {
                println!("{}", report["contract_evidence"]);
                for witness in report["contract_evidence"]["test_candidates"]
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    println!(
                        "Test proposal: {}\nExpected outcome: {}\n{}",
                        witness["file"].as_str().unwrap_or("source"),
                        witness["expected_outcome"]
                            .as_str()
                            .unwrap_or("unavailable"),
                        witness["source"].as_str().unwrap_or("")
                    );
                }
            }
        }
        println!("Finite verification evidence; ordinary compiler checks remain authoritative.");
    }
    Ok(match report["status"].as_str() {
        Some("failed" | "invalid") => 1,
        Some("incomplete" | "conditional") => 2,
        _ if verifying && report["composed"]["status"] == "conditional" => 2,
        _ if verifying && report["commit_eligible"] != true => 1,
        _ => 0,
    })
}

fn verify_batch_command(
    project: &Project,
    request: &std::path::Path,
    json_output: bool,
) -> Result<u8> {
    const MAX_REQUEST: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    fs::File::open(request)
        .map_err(|e| Error::new(0, e.to_string()).in_file(request))?
        .take(MAX_REQUEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::new(0, e.to_string()).in_file(request))?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(Error::new(0, "batch request exceeds 16 MiB").in_file(request));
    }
    let request: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::new(0, format!("invalid batch JSON: {e}")).in_file(request))?;
    // Application and contract execution are explicit JSON choices. The normal
    // verify command's execute-by-default policy does not enter this branch.
    let report = zenith_language::holes::commit_holes(project, &request)?;
    if json_output {
        println!("{report}");
    } else {
        println!(
            "Batch: {}\nSource files written: {}",
            if report["applied"] == true {
                "applied"
            } else if report["commit_eligible"] == true {
                "verified"
            } else {
                report["composed"]["status"].as_str().unwrap_or("invalid")
            },
            report["writes"].as_u64().unwrap_or(0)
        );
        if report["commit_eligible"] != true {
            if let Some(independent) = report["independent"].as_array() {
                for candidate in independent {
                    if candidate["result"]["status"] != "valid" {
                        println!("{candidate}");
                    }
                }
            }
            println!("{}", report["composed"]);
            if !report["contract_evidence"].is_null() {
                println!("{}", report["contract_evidence"]);
            }
        }
    }
    Ok(if report["commit_eligible"] == true {
        0
    } else if report["composed"]["status"] == "conditional"
        || report["contract_evidence"]["status"] == "incomplete"
    {
        2
    } else {
        1
    })
}

fn hot_reload_command(mut args: VecDeque<String>, json_output: bool) -> Result<u8> {
    let baseline = args
        .pop_front()
        .filter(|s| !s.starts_with('-'))
        .ok_or_else(|| Error::new(0, "hot-reload requires BASELINE UPDATED"))?;
    let updated = args
        .pop_front()
        .filter(|s| !s.starts_with('-'))
        .ok_or_else(|| Error::new(0, "hot-reload requires BASELINE UPDATED"))?;
    let mut output = None;
    let mut options = zenith_language::hot_reload::HotReloadOptions::default();
    let mut budget = None;
    while let Some(arg) = args.pop_front() {
        match arg.as_str() {
            "-o" => {
                output = Some(PathBuf::from(
                    args.pop_front()
                        .ok_or_else(|| Error::new(0, "-o requires PATCH.dll"))?,
                ))
            }
            "--release" => options.optimize = true,
            "--rebuild" => options.force = true,
            "--migrate-fn" => {
                if options.migrate_fn.is_some() {
                    return Err(Error::new(0, "provide only one --migrate-fn"));
                }
                options.migrate_fn = Some(args.pop_front().ok_or_else(|| {
                    Error::new(
                        0,
                        "--migrate-fn requires a checked public migration function",
                    )
                })?);
            }
            "--link-lib" => options
                .ffi_libraries
                .push(PathBuf::from(args.pop_front().ok_or_else(|| {
                    Error::new(0, "--link-lib requires FILE.lib")
                })?)),
            "--cache-mb" => {
                let amount = args
                    .pop_front()
                    .ok_or_else(|| Error::new(0, "--cache-mb requires 0..4096"))?
                    .parse::<u64>()
                    .map_err(|_| Error::new(0, "--cache-mb requires 0..4096"))?;
                if amount > 4096 {
                    return Err(Error::new(0, "--cache-mb requires 0..4096"));
                }
                budget = Some(amount * 1024 * 1024);
            }
            _ => return Err(Error::new(0, format!("unknown hot-reload option {arg}"))),
        }
    }
    let baseline = Project::load(&PathBuf::from(baseline))?;
    let updated = Project::load(&PathBuf::from(updated))?;
    options.cache_bytes = budget.unwrap_or(updated.cache_bytes);
    let output = if let Some(output) = output {
        output
    } else {
        use std::hash::{Hash, Hasher};
        let mut generation = std::collections::hash_map::DefaultHasher::new();
        "anode-reload-generation-v2-compiler-digest".hash(&mut generation);
        env!("CARGO_PKG_VERSION").hash(&mut generation);
        backend::compiler_exe_digest()?.hash(&mut generation);
        zenith_language::navigation::project_fingerprint(&baseline).hash(&mut generation);
        zenith_language::navigation::project_fingerprint(&updated).hash(&mut generation);
        options.migrate_fn.hash(&mut generation);
        options.optimize.hash(&mut generation);
        for library in &options.ffi_libraries {
            library.hash(&mut generation);
            fs::read(library)
                .map_err(|e| Error::new(0, e.to_string()).in_file(library))?
                .hash(&mut generation);
        }
        updated.output(false, false, false).with_file_name(format!(
            "{}-reload-{:016x}.dll",
            updated.name,
            generation.finish()
        ))
    };
    for project in [&baseline, &updated] {
        for path in [
            &output,
            &output.with_extension("obj"),
            &output.with_extension("h"),
            &output.with_extension("json"),
        ] {
            protect_sources(project, path)?;
        }
    }
    if updated.managed {
        storage::prepare(&updated)?;
    }
    let patch =
        zenith_language::hot_reload::plan_and_build(&baseline, &updated, &output, &options)?;
    if json_output {
        println!(
            "{}",
            serde_json::json!({"output":patch.output,"object":patch.object,"header":patch.header,"manifest":patch.manifest,
            "symbols":patch.symbols.iter().map(|s|serde_json::json!({"source":s.source_name,"symbol":s.symbol})).collect::<Vec<_>>(),
            "bytes":patch.bytes,"backend_ms":patch.backend_ms,"migration":patch.proof.get("migration")})
        );
    } else {
        println!(
            "Patch: {}\nManifest: {}\n{} compatible entry points; {} bytes",
            patch.output.display(),
            patch.manifest.display(),
            patch.symbols.len(),
            patch.bytes
        );
    }
    Ok(0)
}

fn format_command(args: VecDeque<String>, json_output: bool) -> Result<u8> {
    let mut input = None;
    let mut check = false;
    for arg in args {
        if arg == "--check" {
            check = true;
        } else if arg.starts_with('-') || input.is_some() {
            return Err(Error::new(
                0,
                "fmt accepts [PROJECT|FILE.an] [--check] [--json]",
            ));
        } else {
            input = Some(PathBuf::from(arg));
        }
    }
    let project = Project::load(&input.unwrap_or_else(|| PathBuf::from(".")))?;
    // Produce and validate every result before touching any source file.
    let formatted = zenith_language::fmt::format_project(&project)?;
    let changed: Vec<_> = formatted.iter().filter(|f| f.changed).collect();
    if !check {
        for original in &project.files {
            let current = fs::read_to_string(&original.path)
                .map_err(|e| Error::new(0, e.to_string()).in_file(&original.path))?;
            if current != original.source {
                return Err(Error::new(0, "source changed during formatting; rerun fmt")
                    .in_file(&original.path));
            }
        }
        for (index, file) in changed.iter().enumerate() {
            let original = project
                .files
                .iter()
                .find(|f| f.path == file.path)
                .ok_or_else(|| Error::new(0, "formatter returned an unknown source path"))?;
            replace_formatted_source(&file.path, &original.source, &file.source, index)?;
        }
    }
    if json_output {
        println!(
            "{}",
            serde_json::json!({
                "ok": !check || changed.is_empty(),
                "check": check,
                "checked_files": formatted.len(),
                "changed_files": changed.iter().map(|f| &f.path).collect::<Vec<_>>(),
            })
        );
    } else {
        for file in &changed {
            println!(
                "{} {}",
                if check { "Would format" } else { "Formatted" },
                file.path.display()
            );
        }
        if changed.is_empty() {
            println!("{} source files already formatted", formatted.len());
        }
    }
    Ok(u8::from(check && !changed.is_empty()))
}

fn replace_formatted_source(
    path: &std::path::Path,
    old: &str,
    new: &str,
    index: usize,
) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::new(0, "source has no parent"))?;
    let mut selected = None;
    for attempt in 0..32 {
        let temp = parent.join(format!(
            ".anode-fmt-{}-{index}-{attempt}.tmp",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => {
                selected = Some((temp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::new(0, e.to_string()).in_file(path)),
        }
    }
    let (temp, mut file) = selected
        .ok_or_else(|| Error::new(0, "cannot create a formatting scratch file").in_file(path))?;
    let result = (|| -> std::io::Result<()> {
        file.write_all(new.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&temp, fs::metadata(path)?.permissions())?;
        drop(file);
        if fs::read_to_string(path)? != old {
            return Err(std::io::Error::other(
                "source changed during formatting; rerun fmt",
            ));
        }
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|e| Error::new(0, e.to_string()).in_file(path))
}

fn write_generated_source(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    // Replace the output entry so an existing hardlink cannot truncate its source.
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    for attempt in 0..32 {
        let scratch = parent.join(format!(".anode-emit-{}-{attempt}.tmp", std::process::id()));
        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&scratch)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::new(0, error.to_string()).in_file(path)),
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        let result = written.and_then(|()| fs::rename(&scratch, path));
        if result.is_err() {
            let _ = fs::remove_file(&scratch);
        }
        return result.map_err(|error| Error::new(0, error.to_string()).in_file(path));
    }
    Err(Error::new(0, "cannot create a source emission scratch file").in_file(path))
}

fn protect_sources(project: &Project, output: &std::path::Path) -> Result<()> {
    if let Ok(path) = output.canonicalize() {
        if project.files.iter().any(|f| f.path == path)
            || project.manifest_paths().any(|manifest| {
                manifest
                    .canonicalize()
                    .is_ok_and(|manifest| manifest == path)
            })
        {
            return Err(Error::new(0, "cannot overwrite project source"));
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if env::args().take_while(|a| a != "--").any(|a| a == "--json") {
                println!(
                    "{}",
                    serde_json::json!({"ok":false,"diagnostics":[zenith_language::agent::diagnostic(&error)]})
                );
                return ExitCode::FAILURE;
            }
            eprintln!("{error}");
            if let Some(details) = &error.details {
                if details["kind"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("hot_reload_"))
                {
                    eprintln!(
                        "{}",
                        serde_json::to_string_pretty(details)
                            .unwrap_or_else(|_| details.to_string())
                    );
                }
            }
            if let Some(path) = &error.path {
                if let Ok(source) = fs::read_to_string(path) {
                    if let Some(line) = error
                        .line
                        .checked_sub(1)
                        .and_then(|n| source.lines().nth(n))
                    {
                        eprintln!("  |\n{:>3} | {line}", error.line);
                    }
                }
            }
            ExitCode::FAILURE
        }
    }
}
