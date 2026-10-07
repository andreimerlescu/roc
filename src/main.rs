//! roc binary entry point.

fn main() {
    std::process::exit(roc::cli::main_with_args(std::env::args_os()));
}
