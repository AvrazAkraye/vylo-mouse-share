use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse_cli::CliError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{future::Future, io, process};
use thiserror::Error;
use tokio::task::LocalSet;
use vylo_mouse_share::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};

#[derive(Debug, Error)]
enum VyloError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("VYLO_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn run() -> Result<(), VyloError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => run_daemon(config)?,
        },
        // the Vylo desktop app embeds the service; the bare binary is the headless daemon
        None => run_daemon(config)?,
    }

    Ok(())
}

fn run_daemon(config: Config) -> Result<(), VyloError> {
    match run_async(run_service(config)) {
        Err(VyloError::Service(ServiceError::IpcListen(
            IpcListenerCreationError::AlreadyRunning,
        ))) => {
            log::info!("service already running!");
            Ok(())
        }
        r => r,
    }
}

fn run_async<F, E>(f: F) -> Result<(), VyloError>
where
    F: Future<Output = Result<(), E>>,
    VyloError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
