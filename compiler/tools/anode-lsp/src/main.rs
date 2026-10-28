use anode_lsp::{
    mock_compiler::run_mock_compiler_agent,
    server::{serve_stdio, ServerConfig},
};
use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("agent")
        && env::var("ANODE_MOCK_COMPILER_MODE").is_ok()
    {
        let code = run_mock_compiler_agent(&args);
        return ExitCode::from(code as u8);
    }

    let mut config = ServerConfig::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--version" | "-V" => {
                println!("anode-lsp {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!(
                    "anode-lsp {} — Language Server Protocol server for Anode (.an / .zn)\n\n\
                     Usage:\n  \
                       anode-lsp [--compiler PATH] [--debounce-ms MS] [--stdio]\n\n\
                     Options:\n  \
                       --compiler PATH     Path to bin/anode.exe (overrides ANODE_COMPILER)\n  \
                       --debounce-ms MS    Debounce window in ms for didChange checks (default: 120)\n  \
                       --stdio             Communicate over standard input/output (default)\n  \
                       --version, -V       Print version information\n  \
                       --help, -h          Print this help message",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            "--stdio" => {
                i += 1;
            }
            "--compiler" => {
                let Some(path_str) = args.get(i + 1) else {
                    eprintln!("anode-lsp: --compiler requires a path argument");
                    return ExitCode::FAILURE;
                };
                config.explicit_compiler_path = Some(PathBuf::from(path_str));
                i += 2;
            }
            "--debounce-ms" => {
                let Some(ms_str) = args.get(i + 1) else {
                    eprintln!("anode-lsp: --debounce-ms requires an integer argument");
                    return ExitCode::FAILURE;
                };
                let Ok(ms) = ms_str.parse::<u64>() else {
                    eprintln!("anode-lsp: --debounce-ms must be a non-negative integer");
                    return ExitCode::FAILURE;
                };
                config.debounce_ms = ms.min(5_000);
                i += 2;
            }
            other => {
                eprintln!("anode-lsp: unknown argument '{other}'; use --help");
                return ExitCode::FAILURE;
            }
        }
    }

    let code = serve_stdio(config);
    ExitCode::from(code as u8)
}
