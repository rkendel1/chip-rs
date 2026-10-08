fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env: Vec<(String, String)> = std::env::vars().collect();
    std::process::exit(courier::cli::run(&argv, &env));
}
