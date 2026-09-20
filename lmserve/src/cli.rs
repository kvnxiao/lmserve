use camino::Utf8PathBuf;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use serde::Deserialize;
use serde::Serialize;

#[derive(Parser)]
#[command(version, about)]
pub(crate) struct Cli {
    #[arg(
        long,
        global = true,
        help = "Compose file [default: ./compose.yaml, then the lmserve config directory]"
    )]
    pub(crate) file: Option<Utf8PathBuf>,
    #[arg(long, global = true, default_value = "podman-compose")]
    pub(crate) provider: String,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    List,
    Validate {
        entry: Option<String>,
    },
    Plan {
        #[arg(value_enum)]
        action: Action,
        #[command(flatten)]
        target: Target,
    },
    UpdateImages(Target),
    UpdateModels(Target),
    Start {
        entry: String,
    },
    Stop {
        entry: String,
    },
    Restart {
        entry: String,
    },
    Switch {
        entry: String,
    },
    Status {
        entry: Option<String>,
    },
    Logs {
        entry: String,
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        follow: bool,
    },
    Health {
        entry: String,
    },
    Cdi {
        #[arg(long)]
        output: Option<Utf8PathBuf>,
    },
    #[command(hide = true)]
    Worker {
        operation: String,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
pub(crate) struct Target {
    pub(crate) entry: Option<String>,
    #[arg(long)]
    pub(crate) all: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Action {
    Start,
    Stop,
    Restart,
    Switch,
    UpdateImages,
    UpdateModels,
}
