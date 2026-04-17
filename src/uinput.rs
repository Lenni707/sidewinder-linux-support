// src/uinput.rs
//
// Safe(ish) wrapper around the Linux uinput kernel interface.
//
// We use libc + raw ioctls because:
//   - the `uinput` crate on crates.io is unmaintained/API-unstable
//   - we need fine-grained control over axis ranges and event types
//   - the surface area is small and well-documented in <linux/uinput.h>
//
// All unsafe blocks are isolated here so the rest of the codebase stays safe.

use anyhow::{bail, Context, Result};
use libc::{c_int, c_ulong, open, write, O_NONBLOCK, O_WRONLY};
use std::ffi::CString;
use std::fs::File;
use std::mem;
use std::os::unix::io::{FromRawFd, RawFd};
use std::time::{SystemTime, UNIX_EPOCH};

// ── Linux input subsystem constants ──────────────────────────────────────────
// Sourced from <linux/input.h> and <linux/uinput.h>.
// These are stable ABI values that do not change between kernel versions.

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const EV_FF: u16 = 0x15; // force-feedback (not yet used, reserved)

pub const SYN_REPORT: u16 = 0;

// Absolute axis codes
pub const ABS_X: u16 = 0x00; // steering
pub const ABS_Z: u16 = 0x02; // throttle
pub const ABS_RZ: u16 = 0x05; // brake

// Button codes (gamepad-style mapping)
pub const BTN_TRIGGER: u16 = 0x120; // button 1
pub const BTN_THUMB: u16 = 0x121;   // button 2
pub const BTN_THUMB2: u16 = 0x122;  // button 3
pub const BTN_TOP: u16 = 0x123;     // button 4
pub const BTN_TOP2: u16 = 0x124;    // button 5
pub const BTN_PINKIE: u16 = 0x125;  // button 6
pub const BTN_BASE: u16 = 0x126;    // button 7
pub const BTN_BASE2: u16 = 0x127;   // button 8

// uinput ioctl magic numbers (from <linux/uinput.h>)
const UINPUT_IOCTL_BASE: u8 = b'U';

// UI_SET_EVBIT  = _IOW(UINPUT_IOCTL_BASE, 100, int)
// UI_SET_KEYBIT = _IOW(UINPUT_IOCTL_BASE, 101, int)
// UI_SET_ABSBIT = _IOW(UINPUT_IOCTL_BASE, 103, int)
// UI_DEV_CREATE = _IO (UINPUT_IOCTL_BASE, 1)
// UI_DEV_DESTROY= _IO (UINPUT_IOCTL_BASE, 2)
// UI_ABS_SETUP  = _IOW(UINPUT_IOCTL_BASE, 4, uinput_abs_setup)
//
// We compute these using the standard Linux _IO/_IOW macros manually.

fn iow(nr: u8, size: usize) -> c_ulong {
    // Direction: write = 1 (bits 30-31), size in bits 16-29, type in 8-15, nr in 0-7
    let dir: c_ulong = 1; // _IOC_WRITE
    ((dir << 30) | ((size as c_ulong) << 16) | ((UINPUT_IOCTL_BASE as c_ulong) << 8) | (nr as c_ulong))
}

fn io(nr: u8) -> c_ulong {
    // Direction: none = 0
    (0u64 | ((UINPUT_IOCTL_BASE as c_ulong) << 8) | (nr as c_ulong))
}

fn ui_set_evbit() -> c_ulong  { iow(100, mem::size_of::<c_int>()) }
fn ui_set_keybit() -> c_ulong { iow(101, mem::size_of::<c_int>()) }
fn ui_set_absbit() -> c_ulong { iow(103, mem::size_of::<c_int>()) }
fn ui_dev_create() -> c_ulong { io(1) }
fn ui_dev_destroy() -> c_ulong{ io(2) }
fn ui_abs_setup() -> c_ulong  { iow(4, mem::size_of::<UinputAbsSetup>()) }

// ── Kernel structs (must match layout exactly) ────────────────────────────────

/// Maps to `struct input_id` in <linux/input.h>
#[repr(C)]
struct InputId {
    bustype: u16,
    vendor:  u16,
    product: u16,
    version: u16,
}

/// Maps to `struct uinput_setup` in <linux/uinput.h>
#[repr(C)]
struct UinputSetup {
    id:         InputId,
    name:       [u8; 80], // UINPUT_MAX_NAME_SIZE
    ff_effects_max: u32,
}

/// Maps to `struct uinput_abs_setup` in <linux/uinput.h>
#[repr(C)]
struct UinputAbsSetup {
    code:  u16,
    _pad:  u16,
    info:  InputAbsinfo,
}

/// Maps to `struct input_absinfo` in <linux/input.h>
#[repr(C)]
struct InputAbsinfo {
    value:      i32,
    minimum:    i32,
    maximum:    i32,
    fuzz:       i32,
    flat:       i32,
    resolution: i32,
}

/// Maps to `struct input_event` in <linux/input.h>
#[repr(C)]
pub struct InputEvent {
    pub sec:   i64,
    pub usec:  i64,
    pub type_: u16,
    pub code:  u16,
    pub value: i32,
}

const BUS_USB: u16 = 0x03;

// ── UI_DEV_SETUP ioctl (older kernels use write() instead) ─────────────────

fn ui_dev_setup() -> c_ulong {
    iow(3, mem::size_of::<UinputSetup>())
}

// ── Public virtual device handle ─────────────────────────────────────────────

pub struct VirtualDevice {
    fd: RawFd,
    _file: File, // keeps fd alive; we use RawFd for ioctls
}

impl VirtualDevice {
    /// Create and register a virtual wheel device with the kernel.
    pub fn new() -> Result<Self> {
        // Open /dev/uinput (requires write permission; see udev rules)
        let path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe { open(path.as_ptr(), O_WRONLY | O_NONBLOCK) };
        if fd < 0 {
            bail!(
                "Cannot open /dev/uinput (errno {}). \
                 Make sure your user is in the 'input' group or run with sudo. \
                 See the udev rules section in README.",
                std::io::Error::last_os_error()
            );
        }

        // ── 1. Declare event types ──────────────────────────────────────────
        Self::set_evbit(fd, EV_SYN as i32)?;
        Self::set_evbit(fd, EV_KEY as i32)?;
        Self::set_evbit(fd, EV_ABS as i32)?;

        // ── 2. Declare keys / buttons ───────────────────────────────────────
        for btn in [
            BTN_TRIGGER, BTN_THUMB, BTN_THUMB2, BTN_TOP,
            BTN_TOP2, BTN_PINKIE, BTN_BASE, BTN_BASE2,
        ] {
            Self::set_keybit(fd, btn as i32)?;
        }

        // ── 3. Declare absolute axes ────────────────────────────────────────
        Self::set_absbit(fd, ABS_X as i32)?;
        Self::set_absbit(fd, ABS_Z as i32)?;
        Self::set_absbit(fd, ABS_RZ as i32)?;

        // ── 4. Configure axis ranges via UI_ABS_SETUP ───────────────────────
        // Steering: full signed 16-bit range, normalised to -32768..32767
        Self::abs_setup(fd, ABS_X, -32768, 32767, 128, 0)?;
        // Throttle / brake: 0..255 (unsigned byte from HID pedal)
        Self::abs_setup(fd, ABS_Z,  0, 255, 4, 0)?;
        Self::abs_setup(fd, ABS_RZ, 0, 255, 4, 0)?;

        // ── 5. Fill in device metadata ──────────────────────────────────────
        let mut setup = UinputSetup {
            id: InputId {
                bustype: BUS_USB,
                vendor:  0x045e, // Microsoft VID
                product: 0x001b, // SideWinder FFB Wheel PID (best-guess; adjust as needed)
                version: 1,
            },
            name: [0u8; 80],
            ff_effects_max: 0,
        };
        let name = b"SideWinder Force Feedback Wheel";
        let len = name.len().min(79);
        setup.name[..len].copy_from_slice(&name[..len]);

        let ret = unsafe {
            libc::ioctl(fd, ui_dev_setup(), &setup as *const UinputSetup)
        };
        if ret < 0 {
            bail!("UI_DEV_SETUP ioctl failed: {}", std::io::Error::last_os_error());
        }

        // ── 6. Create the device ────────────────────────────────────────────
        let ret = unsafe { libc::ioctl(fd, ui_dev_create(), 0) };
        if ret < 0 {
            bail!("UI_DEV_CREATE ioctl failed: {}", std::io::Error::last_os_error());
        }

        log::info!("Virtual device created at /dev/input/eventX (check `ls -l /dev/input/` or `evtest`)");

        // Wrap fd in a File so Drop closes it
        let file = unsafe { File::from_raw_fd(fd) };

        Ok(Self { fd, _file: file })
    }

    /// Emit a single input event followed by SYN_REPORT.
    pub fn emit(&self, type_: u16, code: u16, value: i32) -> Result<()> {
        self.write_event(type_, code, value)?;
        self.write_event(EV_SYN, SYN_REPORT, 0)?;
        Ok(())
    }

    /// Write a raw input_event without auto-sync. Use this for batching.
    pub fn write_event(&self, type_: u16, code: u16, value: i32) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ev = InputEvent {
            sec:   now.as_secs() as i64,
            usec:  now.subsec_micros() as i64,
            type_,
            code,
            value,
        };
        let ptr = &ev as *const InputEvent as *const libc::c_void;
        let size = mem::size_of::<InputEvent>();
        let written = unsafe { write(self.fd, ptr, size) };
        if written < 0 {
            bail!("write() to uinput failed: {}", std::io::Error::last_os_error());
        }
        Ok(())
    }

    // ── ioctl helpers ─────────────────────────────────────────────────────────

    fn set_evbit(fd: RawFd, bit: i32) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, ui_set_evbit(), bit) };
        if ret < 0 {
            bail!("UI_SET_EVBIT({}) failed: {}", bit, std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_keybit(fd: RawFd, bit: i32) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, ui_set_keybit(), bit) };
        if ret < 0 {
            bail!("UI_SET_KEYBIT({}) failed: {}", bit, std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_absbit(fd: RawFd, bit: i32) -> Result<()> {
        let ret = unsafe { libc::ioctl(fd, ui_set_absbit(), bit) };
        if ret < 0 {
            bail!("UI_SET_ABSBIT({}) failed: {}", bit, std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn abs_setup(fd: RawFd, code: u16, min: i32, max: i32, fuzz: i32, flat: i32) -> Result<()> {
        let setup = UinputAbsSetup {
            code,
            _pad: 0,
            info: InputAbsinfo {
                value: 0,
                minimum: min,
                maximum: max,
                fuzz,
                flat,
                resolution: 0,
            },
        };
        let ret = unsafe { libc::ioctl(fd, ui_abs_setup(), &setup as *const UinputAbsSetup) };
        if ret < 0 {
            bail!("UI_ABS_SETUP(code={}) failed: {}", code, std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for VirtualDevice {
    fn drop(&mut self) {
        // Destroy the virtual device before the fd is closed.
        unsafe { libc::ioctl(self.fd, ui_dev_destroy(), 0) };
        // _file will be dropped after this, closing the fd.
    }
}
