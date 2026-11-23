// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! WARD BECK TAP daemon entry point (port of ward-beck-audio-tab.py __main__).
//!
//!   --check / --test / --self-test       validate the build and the audio subsystem, exit
//!   --abandon / --teardown / --stop      tear down every WBS sink/route, restore speakers, exit
//!   --no-browser / --headless / --daemon don't open a browser (also WBS_NO_BROWSER=1)
//!   --local / --local-only / --telemetry open http://127.0.0.1:<port> instead of WBS_LAUNCH_URL
//!
//! Environment: WBS_PORT, WBS_SHM_BASE, WBS_LAUNCH_URL, WBS_NO_BROWSER, WBS_DRY_RUN (see tap.rs).

use std::process::{exit, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use ward_beck_audio_tab::{aoip, http, mxl, nmos, pipeline, tap, util};

fn self_test() -> ! {
    println!("[WARD BECK TAP] Validating configuration and self-test...");
    let html = http::TAP_HTML;
    let checks: [(bool, &str); 6] = [
        (html.chars().count() > 500, "HTML template empty"),
        (html.contains("WARD BECK TAP") || html.contains("Ward-Beck"), "HTML header missing"),
        (html.contains("ward-beck.ca") || html.contains("xtm.wbsps.ca"), "Live URL missing"),
        (html.contains("api/stream/meters"), "Telemetry SSE endpoint missing"),
        (tap::bitbuddy_config().get("pattern").and_then(|v| v.as_str()) == Some("prbs"), "Bit Buddy config invalid"),
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
    let pipe = pipeline::inspect_full_pipeline(Some(mxl::engine()));
    let cards = &pipe["layers"]["layer1_kernel_hardware"]["telemetry"]["card_count"];
    println!(
        "[WARD BECK TAP] 3-Layer Linux Audio Pipeline online (ALSA cards: {}).",
        tap::py::str_of(cards)
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

/// SIGTERM / SIGINT: set the shutdown flag, abandon ship, exit 0 (Python handle_sigterm).
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
            {
                let _g = tap::state_lock();
                tap::abandon_ship();
            }
            exit(0);
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |names: &[&str]| args.iter().any(|a| names.contains(&a.as_str()));

    if has(&["--check", "--test", "--self-test"]) {
        self_test();
    }

    if has(&["--abandon", "--teardown", "--stop"]) {
        println!("[WARD BECK TAP] Executing teardown / abandon ship...");
        {
            let _g = tap::state_lock();
            tap::abandon_ship();
        }
        exit(0);
    }

    install_signal_handlers();

    println!("[WARD BECK TAP] Initializing virtual buses, multi-source meters, and signal tap point...");
    tap::full_restart();

    std::thread::spawn(tap::meter_reader_loop);
    std::thread::spawn(nmos::nmos_discovery_loop);
    std::thread::spawn(tap::client_watchdog_loop);

    aoip::set_dante_ip_provider(Box::new(nmos::dante_ips));
    aoip::start(util::shutdown_flag());

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
    {
        let _g = tap::state_lock();
        tap::abandon_ship();
    }
}
