// SPDX-License-Identifier: GPL-3.0-only
//! The `stationd` binary; see [`stationd::cli`].

use std::process::ExitCode;

use clap::Parser as _;
use stationd::cli::{self, Cli, Command, EXIT_FAILURE};

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
    match cli.command {
        Command::CheckConfig { config } => match cli::check_config(&config) {
            Ok(summary) => {
                println!("{summary}");
                ExitCode::SUCCESS
            }
            Err(e) => cli::exit_code(Err(e)),
        },
        Command::Run { config } => runtime.block_on(async {
            let stop = match cli::termination_signal() {
                Ok(stop) => stop,
                Err(e) => return cli::exit_code(Err(e)),
            };
            cli::exit_code(cli::run(&config, stop).await)
        }),
    }
}
