mod auth;
mod java;
mod modpack;
mod version;

use crate::auth::{get_credentials, refresh_credentials};
use crate::java::get_java_settings;
use crate::modpack::get_metadata;
use crate::version::MinecraftVersion;
use crate::UklientError::MetaError;
use auth::get_device_code;
use clap::Parser;
use daedalus::modded::LoaderVersion;
use indicatif::ProgressStyle;
use reqwest::Client;
use std::ffi::OsString;
use tracing::{debug, info, warn};

use libium::HOME;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use theseus::auth::Credentials;
use theseus::data::{MemorySettings, WindowSize};
use theseus::profile;
use theseus::profile::Profile;
use thiserror::Error;
use tokio::sync::oneshot;

type Result<T> = std::result::Result<T, UklientError>;

const FABRIC_META_URL: &str = "https://meta.fabricmc.net/v2";
const QUILT_META_URL: &str = "https://meta.quiltmc.org/v3";
const ONE_SEVENTEEN: MinecraftVersion = MinecraftVersion {
    minor: 17,
    patch: 0,
};
pub static STYLE_BYTE: Lazy<ProgressStyle> = Lazy::new(|| {
    ProgressStyle::default_bar()
        .template("{bytes_per_sec} [{bar:30}] {bytes}/{total_bytes}")
        .expect("Progess bar template parse failure")
        .progress_chars("#>-")
});
pub static CLIENT: Lazy<Client> = Lazy::new(Client::new);

#[derive(Debug, Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(long, default_value_t = String::from("ukupvp"), help = "specify the modpack to be downloaded")]
    modpack_id: String,
    #[arg(long, help = "always download java when launching")]
    force_java_download: bool,
    #[arg(long, help = "don't launch the game, only install the modpack")]
    no_launch: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let format = tracing_subscriber::fmt::format().with_target(false);
    tracing_subscriber::fmt().event_format(format).init();
    let args = Args::parse();

    let game_version = MinecraftVersion::parse("1.19.3")?;
    let java_version: u8 = if game_version >= ONE_SEVENTEEN { 17 } else { 8 };
    let java = get_java_settings(java_version, args.force_java_download).await;

    let metadata =
        get_metadata(&args.modpack_id, game_version.to_string().as_str())
            .await?;
    debug!(
        "Found {} version {:?} on Minecraft {}",
        metadata.loader, metadata.loader_version, game_version
    );

    // grr theseus
    let fixed_name = &metadata.name.replace(' ', "_");
    let base_path: PathBuf = HOME.join(".uklient").join(fixed_name);
    tokio::fs::create_dir_all(&base_path).await?;

    let mc_profile = Profile {
        path: base_path.clone(),
        metadata,
        java: Some(java),
        memory: Some(MemorySettings {
            maximum: (4 * 1024) as u32,
            ..MemorySettings::default()
        }),
        resolution: Some(WindowSize(1280, 720)),
        hooks: None,
    };

    profile::add(mc_profile).await?;
    let cred = connect_account().await?;
    info!("Connected account {}", cred.username);

    modpack::install_modpack(
        &base_path,
        &args.modpack_id,
        game_version.to_string(),
    )
    .await?;
    info!("Sucessfully installed modpack");

    if args.no_launch {
        return Ok(());
    }

    let process = profile::run(&base_path, &cred).await?;
    if let Some(pid) = process.id() {
        info!("PID: {pid}");
    } else {
        warn!("NO PID? no bitches");
    }

    process.wait_with_output().await?;
    info!("Goodbye!");

    Ok(())
}

pub async fn get_latest_fabric(mc_version: &String) -> Result<LoaderVersion> {
    let downloaded = daedalus::download_file(
        format!("{FABRIC_META_URL}/versions/loader/{mc_version}").as_str(),
        None,
    )
    .await?;

    let versions: Vec<LoaderVersionElement> =
        serde_json::from_slice(&downloaded)?;
    let latest = versions.get(0).ok_or(MetaError("fabric"))?.loader.clone();
    let manifest_url = format!(
        "{}/versions/loader/{}/{}/profile/json",
        FABRIC_META_URL, mc_version, latest.version
    );

    Ok(LoaderVersion {
        id: latest.version,
        stable: latest.stable,
        url: manifest_url,
    })
}

pub async fn get_latest_quilt(mc_version: &String) -> Result<LoaderVersion> {
    let downloaded = daedalus::download_file(
        format!("{QUILT_META_URL}/versions/loader/{mc_version}").as_str(),
        None,
    )
    .await?;

    let versions: Vec<LoaderVersionElement> =
        serde_json::from_slice(&downloaded)?;
    let latest = versions.get(0).ok_or(MetaError("quilt"))?.loader.clone();
    let manifest_url = format!(
        "{}/versions/loader/{}/{}/profile/json",
        QUILT_META_URL, mc_version, latest.version
    );

    Ok(LoaderVersion {
        id: latest.version,
        stable: latest.stable,
        url: manifest_url,
    })
}

async fn connect_account() -> Result<Credentials> {
    let credentials_path = Path::new("./credentials.json");

    if credentials_path.try_exists()? {
        let credentials: Result<Credentials> = {
            let file = File::open(credentials_path)?;
            let creds: Credentials =
                serde_json::from_reader(BufReader::new(file))?;

            refresh_credentials(creds).await
        };

        if let Ok(creds) = credentials {
            return Ok(creds);
        }
    }

    let scopes = vec!["XboxLive.signin", "offline_access"];
    let code = get_device_code(scopes).await?;
    warn!(
        "No account was found, please go to {} and enter the code {}",
        code.verification_uri, code.user_code
    );

    let creds = get_credentials(code.device_code).await?;
    let file = File::create(credentials_path)?;
    serde_json::to_writer(BufWriter::new(file), &creds)?;

    Ok(creds)
}

#[derive(Error, Debug)]
#[allow(clippy::enum_variant_names)]
pub enum UklientError {
    #[error("Java could not be located: {0}")]
    JavaLocateError(#[from] java_locator::errors::JavaLocatorError),
    #[error("tokio recv error: {0}")]
    RecvError(#[from] oneshot::error::RecvError),
    #[error("io error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("fs_extra error: {0}")]
    FsExtraError(#[from] fs_extra::error::Error),
    #[error("tokio join error: {0}")]
    JoinError(#[from] tokio::task::JoinError),
    #[error("theseus error: {0}")]
    TheseusError(#[from] theseus::Error),
    #[error("daedalus error: {0}")]
    DaedalusError(#[from] daedalus::Error),
    #[error("json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("libium error: {0}")]
    LibiumError(#[from] libium::upgrade::Error),
    #[error("libium modpack error: {0}")]
    LibiumModpackError(#[from] libium::upgrade::modpack_downloadable::Error),
    #[error("ferinth error: {0}")]
    FerinthError(#[from] ferinth::Error),
    #[error("zip error")]
    ZipError,
    #[error("{0} version not found")]
    MetaError(&'static str),
    #[error("unknown type: {0:?}")]
    UnknownTypeError(OsString),
    #[error("tokio acquire error: {0}")]
    AcquireError(#[from] tokio::sync::AcquireError),
    #[error("reqwest error: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("java not found")]
    JavaNotFoundError,
    #[error("minecraft version error: {0}")]
    VersionError(#[from] crate::version::VersionError),
    #[error("url parse error: {0}")]
    UrlParseError(#[from] url::ParseError),
    #[error("login error: {0}")]
    LoginError(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
/// A version of Minecraft that fabric supports
struct GameVersion {
    /// The version number of the game
    pub version: String,
    /// Whether the Minecraft version is stable or not
    pub stable: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct LoaderVersionElement {
    pub loader: MetaLoaderVersion,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct MetaLoaderVersion {
    /// The separator to get the build number
    pub separator: String,
    /// The build number
    pub build: u32,
    /// The maven artifact
    pub maven: String,
    /// The version number of the fabric loader
    pub version: String,
    /// Whether the loader is stable or not
    #[serde(default = "bool::default")]
    pub stable: bool,
}
