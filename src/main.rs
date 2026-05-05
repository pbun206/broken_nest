use std::env;
use std::fs;
use std::process;

use broken_nest::{Chain, ChainConfig};

fn main() {
    env_logger::init();

    let config_path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: broken-nest <config.toml>");
            process::exit(1);
        }
    };

    let config_str = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read {config_path}: {e}");
            process::exit(1);
        }
    };

    let config: ChainConfig = match toml::from_str(&config_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid config: {e}");
            process::exit(1);
        }
    };

    println!("starting chain with {} plugin(s)...", config.plugins.len());

    let mut chain = match Chain::start(&config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to start chain: {e}");
            process::exit(1);
        }
    };

    if let Some((l, r)) = chain.chain_input_ports() {
        println!("chain input:  {l}, {r}");
    }
    if let Some((l, r)) = chain.chain_output_ports() {
        println!("chain output: {l}, {r}");
    }

    println!("running. Ctrl+C to stop.");

    // Block until Ctrl+C
    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .expect("failed to set Ctrl+C handler");

    rx.recv().ok();

    println!("stopping...");
    chain.stop();
}
