use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use photo_stamp_server::import::{self, ImportOptions};
use photo_stamp_server::overture::Dataset;
use photo_stamp_server::service::{Data, Layout, Service};
use photo_stamp_server::store::Store;

/// The photo-stamp place server: holds the full place data and names points for your family's
/// photo-stamp clients.
#[derive(Parser)]
#[command(version = photo_stamp_http::VERSION)]
struct Cli {
    /// Where the server keeps its data (place data, Overture mirror, accounts)
    #[arg(long, global = true, env = "PHOTO_STAMP_SERVER_DATA", default_value = "/data")]
    data: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Answer clients
    Serve {
        /// Address to listen on (put a TLS proxy, Tailscale or a tunnel in front of it)
        #[arg(long, env = "PHOTO_STAMP_SERVER_LISTEN", default_value = "0.0.0.0:8080")]
        listen: String,
        /// Don't read Overture remotely for files missing from the mirror
        #[arg(long, env = "PHOTO_STAMP_SERVER_OFFLINE")]
        offline: bool,
    },
    /// Bring the place data up to date: mirror Overture, build the places and towns databases
    Import {
        /// Only mirror Overture files covering this box: west,east,south,north (degrees)
        #[arg(long, value_parser = parse_bbox)]
        bbox: Option<[f64; 4]>,
        /// Only mirror these Overture datasets (places, land_use, infrastructure, division)
        #[arg(long, value_delimiter = ',', value_parser = parse_dataset)]
        datasets: Vec<Dataset>,
        /// Don't touch the Overture mirror
        #[arg(long)]
        skip_overture: bool,
        /// Use this places database instead of building one
        #[arg(long)]
        places_from: Option<PathBuf>,
        /// Don't rebuild the places database
        #[arg(long)]
        skip_places: bool,
        /// The places database builder
        #[arg(
            long,
            env = "PHOTO_STAMP_PLACES_SCRIPT",
            default_value = "/usr/share/photo-stamp/build-places-db.py"
        )]
        places_script: PathBuf,
    },
    /// Manage the people who use this server
    User {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Manage devices and their keys
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
}

#[derive(Subcommand)]
enum UserAction {
    /// Add a person
    Add { name: String },
    /// List people
    List,
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Add a device for a person and print its key (shown once)
    Add {
        user: String,
        name: String,
        /// Don't keep the points this device asks about (no trips or map for it)
        #[arg(long)]
        no_history: bool,
    },
    /// List devices
    List,
    /// Stop a device's key working: its id, or person/device
    Revoke { device: String },
}

fn parse_dataset(s: &str) -> Result<Dataset, String> {
    Dataset::from_slug(s.trim())
        .ok_or_else(|| format!("{s}: expected places, land_use, infrastructure or division"))
}

fn parse_bbox(s: &str) -> Result<[f64; 4], String> {
    let v: Vec<f64> =
        s.split(',').map(|x| x.trim().parse::<f64>()).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
    match v.as_slice() {
        &[w, e, s, n] if w < e && s < n => Ok([w, e, s, n]),
        _ => Err("expected west,east,south,north".into()),
    }
}

fn main() -> ExitCode {
    photo_stamp_server::init_logging();
    let cli = Cli::parse();
    let layout = Layout { root: cli.data.clone() };
    match run(cli.command, &layout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command, layout: &Layout) -> anyhow::Result<()> {
    let store = || Store::open(&layout.app_db());
    match command {
        Command::Serve { listen, offline } => {
            // A new server has no place data until its first import finishes.
            if !layout.places().is_file() {
                log::info!("waiting for place data: run `photo-stamp-server import` (the compose file does)");
                while !layout.places().is_file() {
                    std::thread::sleep(Duration::from_secs(30));
                }
            }
            let data = Data::load(layout, offline)?;
            data.gazetteer.warm_up();
            let service = Arc::new(Service::new(store()?, data, layout.clone(), offline));
            service.watch_for_new_data(Duration::from_secs(60));
            let listener = TcpListener::bind(&listen)?;
            log::info!(
                "photo-stamp-server {} listening on {listen} (data in {}, {})",
                photo_stamp_http::VERSION,
                layout.root.display(),
                service.data().data_version()
            );
            photo_stamp_http::serve(listener, Arc::new(move |req| service.handle(req)));
            Ok(())
        }
        Command::Import { bbox, datasets, skip_overture, places_from, skip_places, places_script } => {
            import::run(
                layout,
                &ImportOptions { bbox, datasets, skip_overture, places_from, skip_places, places_script },
            )
        }
        Command::User { action: UserAction::Add { name } } => {
            store()?.add_user(&name)?;
            println!("Added {name}. Next: photo-stamp-server device add {name} <device name>");
            Ok(())
        }
        Command::User { action: UserAction::List } => {
            for (name, devices) in store()?.users()? {
                println!("{name}  ({devices} device{})", if devices == 1 { "" } else { "s" });
            }
            Ok(())
        }
        Command::Device { action: DeviceAction::Add { user, name, no_history } } => {
            let key = store()?.add_device(&user, &name, !no_history)?;
            println!("Device key for {user}'s {name} (shown only this once):\n\n  {key}\n");
            println!("Enter it in the photo-stamp dashboard under Settings > Place server.");
            Ok(())
        }
        Command::Device { action: DeviceAction::List } => {
            for d in store()?.devices()? {
                let seen = d.last_seen.map_or("never".to_owned(), |t| {
                    chrono::DateTime::from_timestamp(t, 0)
                        .map_or(t.to_string(), |d| d.format("%Y-%m-%d %H:%M").to_string())
                });
                println!(
                    "{}  {}/{}  history {}  last seen {seen}{}",
                    d.id,
                    d.user,
                    d.name,
                    if d.history { "on" } else { "off" },
                    if d.revoked { "  (revoked)" } else { "" }
                );
            }
            Ok(())
        }
        Command::Device { action: DeviceAction::Revoke { device } } => {
            store()?.revoke(&device)?;
            println!("Revoked {device}.");
            Ok(())
        }
    }
}
