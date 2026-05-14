use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ktinify")]
#[command(about = "Generate a minimal kernel .config based on runtime coverage and module usage data")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Produce a minimized .config by disabling kernel options unused during the observation period.
    Tinify(TinifyArgs),
}

#[derive(clap::Args)]
struct TinifyArgs {
    /// Path to the kernel source tree; the output .config is written here.
    #[arg(long)]
    kernel_src: PathBuf,

    /// Path to the gcov directory structure gathered from the target system.
    #[arg(long)]
    gcov_dir: PathBuf,

    /// Path to the module usage list produced by ktinify-gather.
    #[arg(long)]
    module_list: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Tinify(args) => {
            if let Err(e) = tinify(args) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn tinify(args: TinifyArgs) -> anyhow::Result<()> {
    todo!()
}
