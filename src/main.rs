fn main() {
    let result = codex_usage_monit::update::maybe_run_proxy()
        .and_then(|code| code.map_or_else(codex_usage_monit::cli::run, Ok));
    let exit_code = match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            1
        }
    };
    std::process::exit(exit_code);
}
