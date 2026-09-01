//! The embedded Vylo service.
//!
//! The full daemon (input capture/emulation, DTLS input channel,
//! clipboard/file sync channel, IPC listener) runs on a dedicated
//! thread inside the app process, on its own single-threaded tokio
//! runtime — the service architecture is `!Send` by design. If another
//! instance already owns the IPC socket, this thread exits quietly and
//! the app's IPC bridge simply connects to the running daemon instead.

use vylo_mouse_share::{
    config::Config,
    service::{Service, ServiceError},
};

pub fn spawn_daemon() {
    // route daemon logs to stderr; VYLO_LOG_LEVEL overrides
    let env = env_logger::Env::default().filter_or("VYLO_LOG_LEVEL", "info");
    let _ = env_logger::Builder::from_env(env).try_init();

    std::thread::Builder::new()
        .name("vylo-daemon".into())
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => return log::error!("daemon runtime: {e}"),
            };
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(run()));
        })
        .expect("failed to spawn daemon thread");
}

async fn run() {
    let config = match Config::embedded() {
        Ok(c) => c,
        Err(e) => return log::error!("failed to load config: {e}"),
    };
    let mut service = match Service::new(config).await {
        Ok(s) => s,
        Err(ServiceError::IpcListen(lan_mouse_ipc::IpcListenerCreationError::AlreadyRunning)) => {
            return log::info!("vylo service already running, connecting to it instead");
        }
        Err(e) => return log::error!("failed to start vylo service: {e}"),
    };
    if let Err(e) = service.run().await {
        log::error!("vylo service exited: {e}");
    }
}
