fn main() {
    let exit_code = match codex_usage_monit::cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    };
    std::process::exit(exit_code);
}
