//! HTTP/1.1 server binary. Configuration path is the only CLI argument.

mod settings;
mod multiplex;
mod ingress;
mod peer;
mod hub;

use std::env;
use std::path::Path;
use std::process;

fn usage(program: &str) {
    eprintln!("usage: {program} <config-file>");
}

fn main() {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "localhost".to_string());
    let config_path = match args.next() {
        Some(p) => p,
        None => {
            usage(&program);
            process::exit(1);
        }
    };

    if args.next().is_some() {
        usage(&program);
        process::exit(1);
    }

    let bundle = match settings::load(Path::new(&config_path)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("localhost: config error: {e}");
            process::exit(1);
        }
    };

    eprintln!(
        "localhost: loaded {} site(s) from {config_path}",
        bundle.sites.len()
    );

    if let Err(e) = hub::run(&bundle) {
        eprintln!("localhost: fatal: {e}");
        process::exit(1);
    }
}
