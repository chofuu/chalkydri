//!
//! Chalkydri core
//!

// Unsafe code is NOT allowed in Chalkydri core.
// If unsafe code is required, it should be part of a different crate.
#![forbid(unsafe_code)]
#![allow(unreachable_code)]

#[macro_use]
extern crate log;
#[cfg(feature = "web")]
extern crate actix_web;
extern crate env_logger;
#[cfg(feature = "ntables")]
extern crate minint;
extern crate tokio;
#[cfg(feature = "web")]
extern crate utoipa as utopia;
#[macro_use]
extern crate serde;
#[cfg(feature = "capriltags")]
extern crate apriltag;
#[cfg(feature = "apriltags")]
extern crate chalkydri_apriltags;
#[cfg(feature = "python")]
extern crate pyo3;
#[cfg(feature = "ml")]
extern crate tfledge;

#[cfg(feature = "web")]
mod api;
mod calibration;
mod cameras;
mod config;
mod error;
mod logger;
mod subsys;
mod subsystem;
mod utils;

#[cfg(feature = "web")]
use api::run_api;
use cameras::{CameraManager};
use config::Config;
use logger::Logger;
use mimalloc::MiMalloc;
#[cfg(feature = "ntables")]
use minint::NtConn;
use once_cell::sync::Lazy;
#[cfg(feature = "rerun")]
use re_sdk::{MemoryLimit, RecordingStream};
#[cfg(feature = "rerun_web_viewer")]
use re_web_viewer_server::WebViewerServerPort;
#[cfg(feature = "rerun")]
use re_ws_comms::RerunServerPort;
use std::{error::Error, net::Ipv4Addr, path::Path, sync::Arc, time::Duration};
#[cfg(feature = "capriltags")]
use subsys::capriltags::CApriltagsDetector;
use tokio::{
    sync::{watch, RwLock},
    task::LocalSet,
};

// mimalloc is a very good general purpose allocator
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use utils::gen_team_ip;

use subsystem::Subsystem;

#[allow(non_upper_case_globals)]
static Cfg: Lazy<RwLock<Config>> = Lazy::new(|| {
    let mut path = Path::new("/boot/chalkydri.toml");
    if !path.exists() {
        path = Path::new("/etc/chalkydri.toml");
        if !path.exists() {
            path = Path::new("./chalkydri.toml");
        }
    }

    RwLock::new(Config::load(path).unwrap())
});

#[cfg(feature = "rerun")]
#[allow(non_upper_case_globals)]
static Rerun: Lazy<RecordingStream> = Lazy::new(|| {
    #[cfg(feature = "rerun_web_viewer")]
    re_sdk::RecordingStreamBuilder::new("chalkydri")
        .serve_web(
            "0.0.0.0",
            WebViewerServerPort(8080),
            RerunServerPort(6969),
            MemoryLimit::from_bytes(10_000_000),
            true,
        )
        .unwrap()
        .into()
});

#[tokio::main(worker_threads = 16)]
async fn main() -> Result<(), Box<dyn Error>> {
    Logger::new().with_path_prefix("logs/handler").init()?;

    gstreamer_base::gst::init().unwrap();

    info!("Chalkydri starting up...");

    let cam_man = CameraManager::new();

    // Create a channel for sharing frames from the camera thread with the subsystems
    let (tx, mut rx) = watch::channel::<Arc<Vec<u8>>>(Arc::new(Vec::new()));

    // Spawn a thread to handle cameras
    let cam_man_ = cam_man.clone();
    std::thread::spawn(move || {
        let tx = tx.clone();
        cam_man_.load_camera(tx).unwrap();
    });

    let api = run_api(cam_man, rx.clone());

    let roborio_ip = {
        let Config {
            ntables_ip,
            team_number,
            ..
        } = &*Cfg.read().await;

        ntables_ip
            .clone()
            .map(|s| {
                s.parse::<Ipv4Addr>()
                    .expect("failed to parse ip address")
                    .octets()
            })
            .unwrap_or_else(|| gen_team_ip(*team_number).expect("failed to generate team ip"))
    };
    // Generate a random device id
    let dev_id = fastrand::u32(..);

    // Attempt to connect to the NT server, retrying until successful

    #[cfg(feature = "ntables")]
    let nt: NtConn;

    #[cfg(feature = "ntables")]
    let mut retry = false;

    #[cfg(feature = "ntables")]
    loop {
        match NtConn::new(roborio_ip, format!("chalkydri{dev_id}")).await {
            Ok(conn) => {
                nt = conn;
                break;
            }
            Err(err) => {
                if !retry {
                    error!("Error connecting to NT server: {err:?}");
                    retry = true;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    info!("Connected to NT server at {roborio_ip:?} successfully!");

    // apriltag C library subsystem
    let local = LocalSet::new();
    #[cfg(feature = "ntables")]
    let nt_ = nt.clone();
    local.spawn_local(async move {
        #[cfg(feature = "ntables")]
        let nt = nt_;

        // Initialize the apriltag C library subsystem
        let mut at = CApriltagsDetector::init().await.unwrap();

        // Publish NT topics

        #[cfg(feature = "ntables")]
        let mut translation = nt
            .publish::<Vec<f64>>(&format!("/chalkydri/robot_pose/translation"))
            .await
            .unwrap();
        #[cfg(feature = "ntables")]
        let mut rotation = nt
            .publish::<Vec<f64>>(&format!("/chalkydri/robot_pose/rotation"))
            .await
            .unwrap();
        #[cfg(feature = "ntables")]
        let mut timestamp = nt
            .publish::<String>(&format!("/chalkydri/robot_pose/timestamp"))
            .await
            .unwrap();

        loop {
            // Wait for a new image from the camera
            if rx.changed().await.is_ok() {
                // Get timestamp for the image
                let ts = chrono::Utc::now().to_rfc3339();
                // Borrow the buffer and let the channel know we've seen this value
                let buf = rx.borrow_and_update();

                // Make a copy of the buffer and release the borrow of the original
                let buf_ = buf.clone();
                drop(buf);

                // Send the buffer to AprilTag detector
                let pose = at.process(buf_).unwrap();

                // Unpack the pose into translation and rotation
                let (t, r) = pose;

                // Update the translation, rotation, and timestamp on NetworkTables
                #[cfg(feature = "ntables")]
                {
                    translation.set(t.clone()).await.unwrap();
                    rotation.set(r.clone()).await.unwrap();
                    timestamp.set(ts).await.unwrap();
                }

                debug!("set vals");
            }
        }
    });

    #[cfg(not(feature = "web"))]
    local.await;

    // Have to let NT topics get dropped before calling nt.stop()
    #[cfg(feature = "web")]
    {
        tokio::join!(
            local,
            api,
        );
    }

    // Shut down NT connection
    #[cfg(feature = "ntables")]
    nt.stop();

    Ok(())
}
