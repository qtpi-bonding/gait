mod codemode_guest;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("codemode-guest") => {
            let check = args.next().as_deref() == Some("--check");
            if let Err(error) = codemode_guest::run(check) {
                eprintln!("codemode guest: {error}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: cargo xtask codemode-guest [--check]");
            std::process::exit(2);
        }
    }
}
