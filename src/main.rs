use std::process::ExitCode;

const USAGE: &str = "\
ttmux — a modern terminal multiplexer

usage: ttmux [options]

    -c, --config <path>   use this config file instead of the default
        --print-config    write the default config to stdout and exit
        --where           print the config path and exit
    -h, --help            show this message
    -V, --version         show the version

Everything else is configured from inside ttmux: press ctrl+a s.
";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("ttmux {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--where" => {
                println!("{}", ttmux::config::config_path().display());
                return ExitCode::SUCCESS;
            }
            "--print-config" => {
                let cfg = ttmux::config::Config::default();
                match toml::to_string_pretty(&cfg) {
                    Ok(s) => print!("{s}"),
                    Err(e) => {
                        eprintln!("ttmux: {e}");
                        return ExitCode::FAILURE;
                    }
                }
                return ExitCode::SUCCESS;
            }
            "-c" | "--config" => match args.next() {
                // The app reads the path from the environment, so this just
                // sets it before startup.
                Some(p) => std::env::set_var("TTMUX_CONFIG", p),
                None => {
                    eprintln!("ttmux: {arg} needs a path");
                    return ExitCode::FAILURE;
                }
            },
            other => {
                eprintln!("ttmux: unknown option {other}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Err(e) = ttmux::app::run() {
        eprintln!("ttmux: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
