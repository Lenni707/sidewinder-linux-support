// src/main.rs
//
// sidewinder-wheel — userspace Linux driver for the Microsoft SideWinder
// Force Feedback Wheel (USB/HID → uinput virtual device).
//
// Architecture:
//
//   hidapi (USB HID access)
//       │  raw HID reports
//       ▼
//   parser::WheelState  ←── byte-level parser (easy to update)
//       │  structured state
//       ▼
//   uinput::VirtualDevice  ──► /dev/input/eventX  ──► games / evtest / jstest

mod parser;
mod uinput;

use anyhow::{bail, Context, Result};
use clap::Parser as ClapParser;
use hidapi::{HidApi, HidDevice};
use parser::{dump_report, WheelState};
use uinput::{
    ABS_RZ, ABS_X, ABS_Z, BTN_BASE, BTN_BASE2, BTN_PINKIE, BTN_THUMB,
    BTN_THUMB2, BTN_TOP, BTN_TOP2, BTN_TRIGGER, EV_ABS, EV_KEY, EV_SYN,
    SYN_REPORT, VirtualDevice,
};

// ── Default USB IDs ───────────────────────────────────────────────────────────
//
// Microsoft VID: 0x045e
// SideWinder Force Feedback Wheel PID: 0x001b
//
// If your wheel has a different PID, use --pid to override, or run --list
// to enumerate all HID devices and find the correct value.
const DEFAULT_VID: u16 = 0x045e;
const DEFAULT_PID: u16 = 0x001b;

// HID read timeout in milliseconds.
// -1 = block forever; 0 = non-blocking. We use a short timeout so the
// main loop can check for Ctrl-C without adding a separate thread.
const READ_TIMEOUT_MS: i32 = 50;

// Maximum expected report size. Oversized buffer is fine; we use actual length.
const REPORT_BUF_SIZE: usize = 64;

// ── CLI ───────────────────────────────────────────────────────────────────────

/// Userspace driver for the Microsoft SideWinder Force Feedback Wheel.
#[derive(ClapParser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// List all HID devices and exit.
    #[arg(long, short)]
    list: bool,

    /// Print raw HID report bytes and parsed values.
    #[arg(long, short)]
    debug: bool,

    /// USB Vendor ID (hex, default: 0x045e = Microsoft).
    #[arg(long, value_parser = parse_hex_u16, default_value = "0x045e")]
    vid: u16,

    /// USB Product ID (hex, default: 0x001b = SideWinder FFB Wheel).
    #[arg(long, value_parser = parse_hex_u16, default_value = "0x001b")]
    pid: u16,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(s, 16).map_err(|e| e.to_string())
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    // Initialise logger. Set RUST_LOG=debug for verbose output, or use --debug.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    let api = HidApi::new().context("Failed to initialise hidapi. Is libhidapi installed?")?;

    if cli.list {
        list_devices(&api);
        return Ok(());
    }

    run_driver(&api, &cli)
}

// ── Device listing ────────────────────────────────────────────────────────────

fn list_devices(api: &HidApi) {
    println!("┌─────────────────────────────────────────────────────────────────┐");
    println!("│  HID device list                                                │");
    println!("└─────────────────────────────────────────────────────────────────┘");

    let mut count = 0;
    for dev in api.device_list() {
        count += 1;
        println!(
            "  VID=0x{:04x}  PID=0x{:04x}  usage_page=0x{:04x}  usage=0x{:04x}",
            dev.vendor_id(),
            dev.product_id(),
            dev.usage_page(),
            dev.usage(),
        );
        println!("    manufacturer : {}", dev.manufacturer_string().unwrap_or("(none)"));
        println!("    product      : {}", dev.product_string().unwrap_or("(none)"));
        println!("    serial       : {}", dev.serial_number().unwrap_or("(none)"));
        println!("    path         : {}", dev.path().to_string_lossy());
        println!();
    }

    if count == 0 {
        println!("  (no HID devices found — check permissions or udev rules)");
    }
}

// ── Driver main loop ──────────────────────────────────────────────────────────

fn run_driver(api: &HidApi, cli: &Cli) -> Result<()> {
    log::info!(
        "Looking for SideWinder wheel: VID=0x{:04x}  PID=0x{:04x}",
        cli.vid, cli.pid
    );

    // Open the HID device
    let device = open_device(api, cli.vid, cli.pid)?;

    log::info!("Device opened successfully.");

    // Set a read timeout so the loop is interruptible (Ctrl-C friendly)
    device
        .set_blocking_mode(false)
        .context("Failed to set non-blocking mode on HID device")?;

    // Create the virtual uinput device (requires /dev/uinput access)
    let vdev = VirtualDevice::new()?;
    log::info!("Virtual input device ready. Run: evtest   or   jstest /dev/input/js0");

    let mut prev_state = WheelState::default();
    let mut buf = [0u8; REPORT_BUF_SIZE];
    let mut consecutive_errors: u32 = 0;

    // ── Main read/forward loop ────────────────────────────────────────────────
    loop {
        match device.read_timeout(&mut buf, READ_TIMEOUT_MS) {
            Ok(0) => {
                // Timeout — no data available, loop again.
                // This is normal with short timeouts; not an error.
                continue;
            }
            Ok(n) => {
                consecutive_errors = 0;
                let report = &buf[..n];

                if cli.debug {
                    dump_report(report);
                }

                // Parse the raw bytes into a structured WheelState.
                if let Some(state) = WheelState::parse(report) {
                    // Forward only changed values to reduce noise in evdev log.
                    forward_state(&vdev, &prev_state, &state, cli.debug)?;
                    prev_state = state;
                } else if cli.debug {
                    // parse() already logged a warning; nothing else to do.
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                log::warn!("HID read error #{}: {}", consecutive_errors, e);

                if consecutive_errors >= 5 {
                    bail!(
                        "Too many consecutive HID read errors ({}). \
                         Device disconnected? Exiting.",
                        consecutive_errors
                    );
                }

                // Brief pause before retrying to avoid a tight error loop
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

// ── Device open helper ────────────────────────────────────────────────────────

fn open_device(api: &HidApi, vid: u16, pid: u16) -> Result<HidDevice> {
    // Try opening by VID/PID. hidapi will pick the first matching interface.
    // On some wheels there are multiple HID interfaces (e.g. FF + input);
    // if this opens the wrong one, use hidapi::DeviceInfo::open_device()
    // filtering by usage_page (0x01 = Generic Desktop, usage 0x04 = joystick).
    match api.open(vid, pid) {
        Ok(dev) => {
            // Print device info for confirmation
            if let Ok(mfr) = dev.get_manufacturer_string() {
                log::info!("  Manufacturer : {}", mfr.unwrap_or_default());
            }
            if let Ok(prod) = dev.get_product_string() {
                log::info!("  Product      : {}", prod.unwrap_or_default());
            }
            Ok(dev)
        }
        Err(e) => {
            // Give a helpful error that explains common causes
            bail!(
                "Could not open HID device VID=0x{:04x} PID=0x{:04x}: {}\n\n\
                 Troubleshooting:\n\
                 • Run `sidewinder-wheel --list` to enumerate devices.\n\
                 • Verify VID/PID with `lsusb | grep -i sidewinder`.\n\
                 • Check permissions: `ls -l /dev/hidraw*`\n\
                 • Add udev rules (see README) or run with: sudo sidewinder-wheel\n\
                 • The wheel may need a different PID — use --pid 0xXXXX to override.",
                vid, pid, e
            )
        }
    }
}

// ── State → uinput event forwarding ──────────────────────────────────────────

/// Compare prev and next WheelState and emit uinput events for changed values.
/// We batch all changed axis/button events, then send a single SYN_REPORT.
fn forward_state(
    vdev: &VirtualDevice,
    prev: &WheelState,
    next: &WheelState,
    debug: bool,
) -> Result<()> {
    let mut changed = false;

    // ── Axes ─────────────────────────────────────────────────────────────────

    if next.steering != prev.steering {
        vdev.write_event(EV_ABS, ABS_X, next.steering as i32)?;
        changed = true;
        if debug {
            println!("  ABS_X (steering)  = {}", next.steering);
        }
    }

    if next.throttle != prev.throttle {
        vdev.write_event(EV_ABS, ABS_Z, next.throttle as i32)?;
        changed = true;
        if debug {
            println!("  ABS_Z (throttle)  = {}", next.throttle);
        }
    }

    if next.brake != prev.brake {
        vdev.write_event(EV_ABS, ABS_RZ, next.brake as i32)?;
        changed = true;
        if debug {
            println!("  ABS_RZ (brake)    = {}", next.brake);
        }
    }

    // ── Buttons (first byte) ─────────────────────────────────────────────────
    // We expose 8 buttons from the first button byte.
    // Button codes in order: TRIGGER, THUMB, THUMB2, TOP, TOP2, PINKIE, BASE, BASE2
    const BUTTON_CODES: [u16; 8] = [
        BTN_TRIGGER, BTN_THUMB, BTN_THUMB2, BTN_TOP,
        BTN_TOP2, BTN_PINKIE, BTN_BASE, BTN_BASE2,
    ];

    for (i, &code) in BUTTON_CODES.iter().enumerate() {
        let was = (prev.buttons >> i) & 1;
        let now = (next.buttons >> i) & 1;
        if was != now {
            vdev.write_event(EV_KEY, code, now as i32)?;
            changed = true;
            if debug {
                println!(
                    "  BTN {} (bit {}) = {}",
                    i,
                    i,
                    if now == 1 { "PRESSED" } else { "released" }
                );
            }
        }
    }

    // ── SYN_REPORT — commit the batch ────────────────────────────────────────
    // Always send SYN_REPORT even if nothing changed, or send only when
    // something changed (we chose "only when changed" to reduce noise).
    if changed {
        vdev.write_event(EV_SYN, SYN_REPORT, 0)?;
    }

    Ok(())
}
