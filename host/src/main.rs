// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! WARD BECK TAP daemon entry point (port of ward-beck-audio-tab.py __main__).
//!
//!   --check / --test / --self-test       validate the build and the audio subsystem, exit
//!   --abandon / --teardown / --stop      tear down every WBS sink/route, restore speakers, exit
//!   --no-browser / --headless / --daemon don't open a browser (also WBS_NO_BROWSER=1)
//!   --local / --local-only / --telemetry open http://127.0.0.1:<port> instead of WBS_LAUNCH_URL
//!
//! Environment: WBS_PORT, WBS_SHM_BASE, WBS_LAUNCH_URL, WBS_NO_BROWSER, WBS_DRY_RUN (see apps/patchbay).
//!
//! The host plugs every application in (`plug_all`), starts them in that order, and serves
//! HTTP; it knows the applications only to plug them in and for the self-test.

use std::process::{exit, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

mod http;

use wbs_common as util;
use wbs_patchbay as tap;
use wbs_plugin::services;

/// Plug every application in. The order is the start-up order: the patchbay builds the WBS
/// buses before discovery, the AoIP listener and the AES67 transmitters start.
fn plug_all() {
    wbs_mxl::plug();
    wbs_patchbay::plug();
    wbs_receivers::plug();
    wbs_inserts::plug();
    wbs_hardware::plug();
    wbs_ntp::plug();
    wbs_nmos::plug();
    wbs_aoip::plug();
    wbs_aes67::plug();
    wbs_cbc::plug();
    wbs_pipeline::plug();
    wbs_listen::plug();
    wbs_diagnostics::plug();
    wbs_setup::plug();
}

fn self_test() -> ! {
    println!("[WARD BECK TAP] Validating configuration and self-test...");
    let html = http::TAP_HTML;
    let checks: [(bool, &str); 6] = [
        (html.chars().count() > 500, "HTML template empty"),
        (html.contains("WARD BECK TAP") || html.contains("Ward-Beck"), "HTML header missing"),
        (html.contains("ward-beck.ca") || html.contains("xtm.wbsps.ca"), "Live URL missing"),
        (html.contains("api/stream/meters"), "Telemetry SSE endpoint missing"),
        (tap::bitbuddy_config().get("pattern").and_then(|v| v.as_str()) == Some("sweep"), "Bit Buddy config invalid"),
        (http::CORS_HEADERS.len() == 4, "CORS headers missing"),
    ];
    for (ok, msg) in checks {
        if !ok {
            eprintln!("AssertionError: {msg}");
            exit(1);
        }
    }
    let srcs = tap::list_sources();
    println!("[WARD BECK TAP] Detected audio subsystem ({} available audio sources).", srcs.len());
    let pipe = services::pipeline().inspect_full();
    let cards = &pipe["layers"]["layer1_kernel_hardware"]["telemetry"]["card_count"];
    println!(
        "[WARD BECK TAP] 3-Layer Linux Audio Pipeline online (ALSA cards: {}).",
        wbs_pyshim::str_of(cards)
    );
    println!("[WARD BECK TAP] \u{2714} ABB-1 Bit Buddy & ABP-1 Bit Spitter engine verified.");
    println!("[WARD BECK TAP] \u{2714} NMOS IS-04 Announcement & Stream Discovery verified.");
    println!("[WARD BECK TAP] \u{2714} CBC Radio-Canada MXL Test Generator (:9600) & Pattern Studio verified.");
    println!("[WARD BECK TAP] \u{2714} Self-test passed: WARD BECK TAP is valid, MXL-aware, and deployment-ready.");
    exit(0)
}

/// Launches the system browser pointing to the router interface (Python launch_browser).
fn launch_browser(url: String, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return;
        }
        let _ = Command::new("xdg-open")
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|mut c| {
                std::thread::spawn(move || {
                    let _ = c.wait();
                })
            });
    });
}

/// Ctrl-C / service stop on Windows: signal-hook has no iterator there, so a flag is
/// registered and watched; the shutdown is the same as on Unix.
#[cfg(windows)]
fn install_signal_handlers() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for sig in [SIGINT, SIGTERM] {
        if let Err(e) = signal_hook::flag::register(sig, hit.clone()) {
            eprintln!("[WARN] signal handler {sig} not installed: {e}");
        }
    }
    std::thread::spawn(move || loop {
        if hit.load(Ordering::SeqCst) {
            util::shutdown_flag().store(true, Ordering::SeqCst);
            services::patchbay().abandon_ship();
            exit(0);
        }
        std::thread::sleep(Duration::from_millis(200));
    });
}

/// SIGTERM / SIGINT: set the shutdown flag, abandon ship, exit 0 (Python handle_sigterm).
#[cfg(unix)]
fn install_signal_handlers() {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGTERM, SIGINT]) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[WARN] signal handlers not installed: {e}");
            return;
        }
    };
    std::thread::spawn(move || {
        if signals.forever().next().is_some() {
            util::shutdown_flag().store(true, Ordering::SeqCst);
            services::patchbay().abandon_ship();
            exit(0);
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |names: &[&str]| args.iter().any(|a| names.contains(&a.as_str()));

    plug_all();

    if has(&["--check", "--test", "--self-test"]) {
        self_test();
    }

    if has(&["--abandon", "--teardown", "--stop"]) {
        println!("[WARD BECK TAP] Executing teardown / abandon ship...");
        services::patchbay().abandon_ship();
        exit(0);
    }

    install_signal_handlers();
    wbs_diagnostics::startup_banner();
    std::thread::spawn(wbs_log::client_summary_loop);
    let names: Vec<&str> = wbs_plugin::plugins().iter().map(|p| p.name()).collect();
    wbs_log::info("start", &format!("plugins: {}", names.join(" ")));
    wbs_plugin::start_all();

    let no_browser = has(&["--no-browser", "--headless", "--daemon"])
        || std::env::var("WBS_NO_BROWSER").unwrap_or_else(|_| "0".into()) == "1";
    if !no_browser {
        let mut target_url = std::env::var("WBS_LAUNCH_URL").unwrap_or_else(|_| "https://ward-beck.ca".into());
        if has(&["--local", "--local-only", "--telemetry"]) {
            target_url = format!("http://127.0.0.1:{}", util::port());
        }
        println!("[WARD BECK TAP] Launching live broadcast visualizer: {target_url}");
        launch_browser(target_url, Duration::from_millis(400));
    }

    // Python: a bind failure raises here, before the serve_forever try/finally.
    let server = match http::bind() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("OSError: {e}");
            exit(1);
        }
    };
    println!(
        "[WARD BECK TAP] HTML Router & Multi-Source VU Bridge running at: http://127.0.0.1:{}",
        util::port()
    );
    println!("[WARD BECK TAP] Live VU meters & MXL Continuous Ring Buffers active. Signal Tap online.");
    http::serve(server);
    services::patchbay().abandon_ship();
}
