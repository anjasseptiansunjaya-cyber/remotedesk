mod capture;
mod keymap;
#[cfg(target_os = "linux")]
mod portal;
mod server;
#[cfg(windows)]
mod win;

#[cfg(target_os = "linux")]
pub use portal::Portal as Host;
#[cfg(windows)]
pub use win::Host;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use rand::RngExt;
use tokio::sync::watch;

struct Args {
    port: u16,
    password: Option<String>,
    fps: u32,
    quality: u32,
    bitrate: u32,
    max_width: i32,
    codec: capture::Codec,
    grab: capture::Grab,
}

fn parse_args() -> Args {
    let mut a = Args { port: 8443, password: None, fps: 30, quality: 70, bitrate: 4000, max_width: 1920, codec: capture::Codec::Vp8, grab: capture::Grab::Dd };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next();
        match (k.as_str(), v) {
            ("--port", Some(v)) => a.port = v.parse().unwrap_or(a.port),
            ("--password", Some(v)) => a.password = Some(v),
            ("--fps", Some(v)) => a.fps = v.parse().unwrap_or(a.fps),
            ("--quality", Some(v)) => a.quality = v.parse().unwrap_or(a.quality),
            ("--bitrate", Some(v)) => a.bitrate = v.parse().unwrap_or(a.bitrate),
            ("--codec", Some(v)) => a.codec = if v == "jpeg" { capture::Codec::Jpeg } else { capture::Codec::Vp8 },
            ("--grab", Some(v)) => a.grab = if v == "gdigrab" { capture::Grab::Gdi } else { capture::Grab::Dd },
            ("--max-width", Some(v)) => a.max_width = v.parse().unwrap_or(a.max_width),
            _ => {
                eprintln!("pemakaian: remotedesk [--port 8443] [--password PW] [--fps 30] [--codec vp8|jpeg] [--bitrate 4000 (kbps, vp8)] [--quality 70 (jpeg)] [--max-width 1920] [--grab ddagrab|gdigrab (Windows)]");
                std::process::exit(2);
            }
        }
    }
    a
}

fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let dir = PathBuf::from(std::env::var("APPDATA").context("APPDATA tidak ada")?).join("remotedesk");
    #[cfg(not(windows))]
    let dir = PathBuf::from(std::env::var("HOME").context("HOME tidak ada")?).join(".config/remotedesk");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn load_password(dir: &PathBuf, given: Option<String>) -> Result<String> {
    if let Some(p) = given {
        return Ok(p);
    }
    let f = dir.join("password");
    if let Ok(p) = std::fs::read_to_string(&f) {
        return Ok(p.trim().to_string());
    }
    const CH: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let p: String = (0..10).map(|_| CH[rng.random_range(0..CH.len())] as char).collect();
    std::fs::write(&f, &p)?;
    Ok(p)
}

/// The address of the interface that routes to the internet (no packet is sent).
fn local_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

#[cfg(windows)]
fn check_ffmpeg() {
    let bundled = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("ffmpeg.exe")));
    let cmd = bundled.filter(|p| p.exists()).unwrap_or_else(|| "ffmpeg".into());
    let ok = std::process::Command::new(cmd).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        eprintln!("PERINGATAN: ffmpeg.exe tidak ditemukan. Taruh ffmpeg.exe di folder yang sama dengan remotedesk.exe");
        eprintln!("            (unduh build 'essentials' dari https://www.gyan.dev/ffmpeg/builds/).");
    }
}

fn load_cert(dir: &PathBuf) -> Result<(PathBuf, PathBuf)> {
    let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !c.exists() || !k.exists() {
        let mut names = vec!["localhost".to_string()];
        names.extend(local_ip());
        #[cfg(target_os = "linux")]
        if let Ok(out) = std::process::Command::new("hostname").arg("-I").output() {
            names.extend(String::from_utf8_lossy(&out.stdout).split_whitespace().map(String::from));
        }
        names.sort();
        names.dedup();
        let ck = rcgen::generate_simple_self_signed(names)?;
        std::fs::write(&c, ck.cert.pem())?;
        std::fs::write(&k, ck.signing_key.serialize_pem())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok((c, k))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("gagal memasang crypto provider"))?;

    let dir = config_dir()?;
    let password = load_password(&dir, args.password)?;
    let (cert, key) = load_cert(&dir)?;

    #[cfg(target_os = "linux")]
    let (host, source) = {
        let host = Host::start(&dir.join("restore_token")).await?;
        // The capture task needs its own handle to the PipeWire fd.
        let source = capture::Source { size: host.size, fd: host.pw_fd.try_clone()?, node: host.node_id };
        (host, source)
    };
    #[cfg(windows)]
    let (host, source) = {
        check_ffmpeg();
        let host = Host::start(args.grab)?;
        let source = capture::Source { size: host.size };
        (host, source)
    };
    let frame_size = capture::out_size(host.size, args.max_width);

    let (frames_tx, frames_rx) = watch::channel(capture::empty_gop());
    let (viewers_tx, viewers_rx) = watch::channel(0usize);
    let viewers_tx = Arc::new(viewers_tx);

    tokio::spawn(capture::run(
        source,
        capture::Opts {
            codec: args.codec,
            #[cfg(windows)]
            grab: args.grab,
            fps: args.fps,
            quality: args.quality,
            bitrate_kbps: args.bitrate,
            max_width: args.max_width,
        },
        viewers_rx,
        frames_tx,
    ));

    let app = server::router(server::AppState {
        password: Arc::new(password.clone()),
        host: Arc::new(host),
        frames: frames_rx,
        viewers: viewers_tx,
        frame_size,
        codec: args.codec.name(),
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    println!("RemoteDesk siap.");
    println!("  Buka dari browser:  https://{}:{}", local_ip().unwrap_or("<IP-komputer-ini>".into()), args.port);
    println!("  Password:           {password}");
    println!("  (browser akan memberi peringatan sertifikat self-signed, pilih lanjutkan)");
    let tls = RustlsConfig::from_pem_file(cert, key).await?;
    axum_server::bind_rustls(addr, tls).serve(app.into_make_service()).await?;
    Ok(())
}
