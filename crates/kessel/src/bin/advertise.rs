//! Publish a host into the discovery listing and hold the connection open.
//!
//! Phase 4 gate 1 in the smallest runnable form: connect as a host, advertise
//! once, and stay up so the service keeps treating us as online. Everything
//! inbound is reported rather than acted on, because admission is the next
//! increment and there is nothing to admit a guest to yet.
//!
//!   KESSEL_WS_SERVER=wss://... KESSEL_SESSION=... advertise [--name NAME]

use std::env;

use lowlat_kessel::message::ConnUpdate;
use lowlat_kessel::{Client, Connect, Role};

/// The generation this host implements, as the wire wants it: a string.
const APP_V: &str = "150-104a";

/// Matches the SDK generation the opcode set belongs to.
const SDK_V: u32 = 0x0006_0000;

/// Advertised capacity comes from the configured guest limit, never a constant
/// larger than admission will grant.
const MAX_GUESTS: u32 = 4;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("advertise: {error}");
        std::process::exit(1);
    }
}

fn flag(name: &str) -> Option<String> {
    let args: Vec<String> = env::args().collect();
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // Configuration carries a bare host as often as a URL, and a scheme-less
    // one produces a refused upgrade rather than anything that reads as a
    // missing scheme.
    let configured = env::var("KESSEL_WS_SERVER").map_err(|_| "KESSEL_WS_SERVER is not set")?;
    let configured = configured.trim();
    let server = if configured.contains("://") {
        configured.to_string()
    } else {
        format!("wss://{configured}")
    };
    let session = env::var("KESSEL_SESSION").map_err(|_| "KESSEL_SESSION is not set")?;

    let hostname = read("/proc/sys/kernel/hostname");
    let name = flag("--name").unwrap_or_else(|| {
        if hostname.is_empty() {
            "lowlat".to_string()
        } else {
            hostname.clone()
        }
    });

    let mut client = Client::connect(&Connect {
        server,
        session_id: session,
        role: Role::Host,
        build: APP_V.to_string(),
        sdk_version: SDK_V,
    })
    .await?;

    // Emitted on state change, and connecting is one. Never on a timer: the
    // service derives liveness from the connection, so a periodic advertisement
    // adds load and buys nothing.
    let advertisement = ConnUpdate {
        loader_v: 0,
        service_v: 0,
        os: "linux".to_string(),
        os_v: read("/proc/sys/kernel/osrelease"),
        platform: "linux".to_string(),
        app_v: APP_V.to_string(),
        sdk_v: SDK_V,
        device_id: read("/etc/machine-id"),
        mode: "desktop".to_string(),
        name: name.clone(),
        desc: String::new(),
        game_id: String::new(),
        secret: String::new(),
        max_players: MAX_GUESTS,
        players: 0,
        is_public: false,
        guests: Vec::new(),
    };

    println!("advertising as {name:?}, capacity {MAX_GUESTS}");
    println!(
        "{}",
        lowlat_kessel::message::envelope("conn_update", &advertisement)?
    );
    client.send("conn_update", &advertisement)?;

    println!("connected; refresh the client listing. ctrl-c to stop.");
    loop {
        tokio::select! {
            message = client.recv() => match message {
                Some(message) => {
                    println!("<- {} {}", message.action, message.payload);
                }
                None => {
                    println!("connection closed by the service");
                    return Ok(());
                }
            },
            _ = tokio::signal::ctrl_c() => {
                println!("stopping");
                return Ok(());
            }
        }
    }
}
