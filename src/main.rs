use std::env;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{ArgAction, Parser};

#[derive(Debug, Parser)]
#[command(author, version, about = "Run an app in a virtual display and stream it to a browser")]
struct Cli {
    /// Display number for Xvfb (e.g. 99 means :99)
    #[arg(long, default_value_t = 99)]
    display: u16,

    /// Automatically pick a free display number starting at --display
    #[arg(long, action = ArgAction::SetTrue)]
    auto_display: bool,

    /// Virtual screen size as WIDTHxHEIGHTxDEPTH (e.g. 1280x720x24)
    #[arg(long, default_value = "1280x720x24")]
    screen: String,

    /// Extra argument to pass directly to Xvfb (repeatable)
    #[arg(long = "xvfb-arg")]
    xvfb_args: Vec<String>,

    /// TCP port where x11vnc listens for VNC clients
    #[arg(long, default_value_t = 5900)]
    vnc_port: u16,

    /// TCP port where websockify exposes websocket for noVNC
    #[arg(long, default_value_t = 6080)]
    ws_port: u16,

    /// TCP port for the built-in web UI
    #[arg(long, default_value_t = 8080)]
    web_port: u16,

    /// Optional bind host for web UI and websocket endpoints
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Print child process output passthrough
    #[arg(long, action = ArgAction::SetTrue)]
    verbose: bool,

    /// Disable browser streaming and run as a plain xvfb-style wrapper
    #[arg(long, action = ArgAction::SetTrue)]
    no_browser: bool,

    /// Command to run after --
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from((code & 0xff) as u8),
        Err(err) => {
            eprintln!("error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<i32> {
    if std::env::var("NOSEBLEED_X11_HANDSHAKE_ONLY").is_ok() {
        // Developer hook: run the minimal handshake server on 6000 for local experiments.
        nosebleed::run_single_handshake("127.0.0.1:6000")?;
        return Ok(0);
    }

    let cli = Cli::parse();

    if cli.command.is_empty() {
        bail!("missing target command. usage: nosebleed [opts] -- <command> [args...]");
    }

    ensure_binary_exists("Xvfb")?;
    if !cli.no_browser {
        ensure_binary_exists("x11vnc")?;
        ensure_binary_exists("websockify")?;
    }

    let running = Arc::new(AtomicBool::new(true));
    let (display, mut xvfb) = start_xvfb(&cli).context("starting Xvfb")?;

    let mut x11vnc = None;
    let mut websockify = None;
    let mut server_thread = None;

    if !cli.no_browser {
        let started_x11vnc =
            spawn_x11vnc(&display, cli.vnc_port, cli.verbose).context("starting x11vnc")?;
        let started_websockify = spawn_websockify(&cli.host, cli.ws_port, cli.vnc_port, cli.verbose)
            .context("starting websockify")?;

        let web_running = Arc::clone(&running);
        let ws_url = format!("ws://{}:{}", cli.host, cli.ws_port);
        let web_host = cli.host.clone();
        let web_port = cli.web_port;
        let started_server_thread = thread::spawn(move || {
            if let Err(err) = run_web_server(&web_host, web_port, &ws_url, web_running) {
                eprintln!("web ui error: {err:#}");
            }
        });

        x11vnc = Some(started_x11vnc);
        websockify = Some(started_websockify);
        server_thread = Some(started_server_thread);
    }

    let mut child_cmd = Command::new(&cli.command[0]);
    child_cmd.args(&cli.command[1..]);
    child_cmd.env("DISPLAY", &display);

    // Keep runtime dirs stable for apps that expect them.
    if let Ok(xdg_runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        child_cmd.env("XDG_RUNTIME_DIR", xdg_runtime_dir);
    }

    if cli.verbose {
        child_cmd.stdout(std::process::Stdio::inherit());
        child_cmd.stderr(std::process::Stdio::inherit());
    }

    let mut target = child_cmd
        .spawn()
        .with_context(|| format!("spawning target command: {}", cli.command.join(" ")))?;

    println!("nosebleed started");
    println!("display: {display}");
    if !cli.no_browser {
        println!("open browser: http://{}:{}", cli.host, cli.web_port);
    }

    let terminate = Arc::clone(&running);
    ctrlc::set_handler(move || {
        terminate.store(false, Ordering::SeqCst);
    })
    .context("installing ctrl-c handler")?;

    let exit_code = loop {
        if !running.load(Ordering::SeqCst) {
            kill_if_running(&mut target);
            break 130;
        }

        if let Some(status) = target.try_wait().context("checking target process status")? {
            break status.code().unwrap_or(1);
        }

        thread::sleep(Duration::from_millis(100));
    };

    running.store(false, Ordering::SeqCst);
    if let Some(mut child) = websockify {
        kill_if_running(&mut child);
    }
    if let Some(mut child) = x11vnc {
        kill_if_running(&mut child);
    }
    kill_if_running(&mut xvfb);

    if let Some(handle) = server_thread {
        let _ = handle.join();
    }

    Ok(exit_code)
}

fn start_xvfb(cli: &Cli) -> Result<(String, Child)> {
    if cli.auto_display {
        for candidate in cli.display..(cli.display + 100) {
            if !x_socket_exists(candidate) {
                let display = format!(":{candidate}");
                if let Ok(child) = spawn_xvfb(&display, &cli.screen, &cli.xvfb_args) {
                    return Ok((display, child));
                }
            }
        }
        bail!("could not find a free display in range {}..{}", cli.display, cli.display + 99);
    }

    let display = format!(":{}", cli.display);
    let child = spawn_xvfb(&display, &cli.screen, &cli.xvfb_args)?;
    Ok((display, child))
}

fn spawn_xvfb(display: &str, screen: &str, extra_args: &[String]) -> Result<Child> {
    let mut cmd = Command::new("Xvfb");
    cmd.arg(display)
        .arg("-screen")
        .arg("0")
        .arg(screen)
        .arg("-nolisten")
        .arg("tcp")
        .args(extra_args);

    let mut child = cmd.spawn().context("Xvfb failed to spawn")?;
    thread::sleep(Duration::from_millis(150));
    if let Some(status) = child.try_wait().context("waiting for Xvfb startup")? {
        bail!("Xvfb exited early for {display} with status {status}");
    }
    Ok(child)
}

fn spawn_x11vnc(display: &str, vnc_port: u16, verbose: bool) -> Result<Child> {
    let mut cmd = Command::new("x11vnc");
    cmd.arg("-display")
        .arg(display)
        .arg("-rfbport")
        .arg(vnc_port.to_string())
        .arg("-nopw")
        .arg("-forever")
        .arg("-shared")
        .arg("-xkb");

    if !verbose {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }

    cmd.spawn().context("x11vnc failed to spawn")
}

fn spawn_websockify(host: &str, ws_port: u16, vnc_port: u16, verbose: bool) -> Result<Child> {
    let mut cmd = Command::new("websockify");
    cmd.arg(format!("{host}:{ws_port}"))
        .arg(format!("127.0.0.1:{vnc_port}"));

    if !verbose {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }

    cmd.spawn().context("websockify failed to spawn")
}

fn run_web_server(host: &str, port: u16, ws_url: &str, running: Arc<AtomicBool>) -> Result<()> {
    let listener = TcpListener::bind((host, port)).with_context(|| format!("binding web ui on {host}:{port}"))?;
    listener
        .set_nonblocking(true)
        .context("setting nonblocking listener")?;

    while running.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let mut buf = [0_u8; 1024];
                let _ = stream.read(&mut buf);

                let body = render_html(ws_url);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );

                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(err).context("accepting web ui connection"),
        }
    }

    Ok(())
}

fn render_html(ws_url: &str) -> String {
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>nosebleed</title>
  <style>
    :root {{
      color-scheme: dark;
      --bg: #0f1216;
      --panel: #1a1f26;
      --text: #e9eef4;
      --muted: #8f9aab;
      --accent: #44d17a;
    }}
    html, body {{ height: 100%; margin: 0; background: radial-gradient(circle at 20% 0%, #1e2632 0%, #0f1216 55%); color: var(--text); font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace; }}
    .wrap {{ display: grid; grid-template-rows: auto 1fr; height: 100%; }}
    .bar {{ padding: 10px 14px; background: rgba(0, 0, 0, 0.35); border-bottom: 1px solid #2f3946; display: flex; gap: 12px; align-items: baseline; }}
    .title {{ font-weight: 700; color: var(--accent); letter-spacing: 0.05em; text-transform: uppercase; font-size: 12px; }}
    .meta {{ color: var(--muted); font-size: 12px; }}
    #screen {{ width: 100%; height: 100%; background: #000; }}
  </style>
</head>
<body>
  <div class="wrap">
    <div class="bar">
      <div class="title">nosebleed</div>
      <div class="meta">{ws_url}</div>
    </div>
    <div id="screen"></div>
  </div>

  <script type="module">
    import RFB from 'https://cdn.jsdelivr.net/npm/@novnc/novnc@1.5.0/core/rfb.js';

    const target = document.getElementById('screen');
    const rfb = new RFB(target, '{ws_url}');
    rfb.viewOnly = false;
    rfb.scaleViewport = true;
    rfb.resizeSession = true;
    rfb.background = '#000';
    rfb.focusOnClick = true;

    rfb.addEventListener('connect', () => console.log('connected'));
    rfb.addEventListener('disconnect', (e) => console.log('disconnected', e.detail?.clean));
  </script>
</body>
</html>"#
    )
}

fn kill_if_running(child: &mut Child) {
    if let Ok(None) = child.try_wait() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn x_socket_exists(display: u16) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{display}")).exists()
}

fn ensure_binary_exists(name: &str) -> Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name} >/dev/null 2>&1"))
        .status()
        .with_context(|| format!("checking dependency: {name}"))?;

    if !status.success() {
        bail!("required dependency not found in PATH: {name}");
    }

    Ok(())
}
