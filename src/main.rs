fn main() -> std::process::ExitCode {
    acs::run(std::env::args_os().collect())
}
