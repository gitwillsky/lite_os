fn main() -> std::process::ExitCode {
    match network_config::serve_router_api() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("router-api: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
