// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` binary; see [`stationd::cli`].

use std::process::ExitCode;

use clap::Parser as _;
use stationd::cli::{self, Cli, EXIT_FAILURE};

fn main() -> ExitCode {
    let cli = Cli::parse();
    stationd::logging::init();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("stationd: cannot start the async runtime: {e}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    cli::main(cli.command, &runtime)
}
