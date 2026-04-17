# sidewinder-wheel

Userspace Linux driver for the **Microsoft SideWinder Force Feedback Wheel** (USB).

Reads raw HID reports from the wheel and re-exposes it as a standard Linux
virtual input device (`/dev/input/eventX`) via **uinput**, making it usable
by any game or tool that works with evdev/joystick devices.

---

## Architecture

```
hidapi  →  parser::WheelState  →  uinput::VirtualDevice  →  /dev/input/eventX
(USB)      (byte-level parser)    (kernel uinput ioctls)     (games, evtest, jstest)
```

**Files:**

| File | Purpose |
|---|---|
| `src/main.rs` | CLI, device discovery, read/forward loop |
| `src/parser.rs` | Raw HID report → `WheelState` struct |
| `src/uinput.rs` | Safe wrapper around Linux uinput ioctls |
| `99-sidewinder-wheel.rules` | udev permission rules |

---

## Build

### Prerequisites

```bash
# Debian/Ubuntu
sudo apt install libhidapi-dev libhidapi-hidraw0 build-essential

# Fedora
sudo dnf install hidapi-devel

# Arch
sudo pacman -S hidapi
```

Install Rust if needed:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

### Compile

```bash
cd sidewinder-wheel
cargo build --release
# Binary: target/release/sidewinder-wheel
```

For development (faster compile, debug symbols):
```bash
cargo build
# Binary: target/debug/sidewinder-wheel
```

---

## Permissions setup (one-time)

```bash
# Install udev rules
sudo cp 99-sidewinder-wheel.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger

# Add yourself to the 'input' group
sudo usermod -aG input $USER

# Log out and back in (or: newgrp input in your current shell)
```

After this, you can run without `sudo`. Until then, prefix commands with `sudo`.

---

## Run

### List all HID devices (no wheel needed yet)
```bash
./target/release/sidewinder-wheel --list
```
Look for your wheel in the output. Note its VID and PID.

### Start the driver (default VID/PID)
```bash
./target/release/sidewinder-wheel
```

### With a different PID
```bash
./target/release/sidewinder-wheel --pid 0x001c
```

### Debug mode (shows raw bytes + parsed values)
```bash
./target/release/sidewinder-wheel --debug
```

### Enable verbose logging
```bash
RUST_LOG=debug ./target/release/sidewinder-wheel --debug
```

---

## Verifying the virtual device

### evtest (most informative)
```bash
sudo evtest
# Select the "SideWinder Force Feedback Wheel" entry
# Turn wheel, press pedals, press buttons — events appear live
```

### jstest (joystick-style)
```bash
# Find the joystick node
ls /dev/input/js*
jstest /dev/input/js0
# or: jstest-gtk (GUI version)
```

### libinput
```bash
sudo libinput debug-events --show-keycodes
# Generates structured event output; useful for verifying axis normalization
```

---

## Inspecting the HID side (for parser tuning)

### Identify the device
```bash
lsusb | grep -i "045e"           # list Microsoft USB devices
lsusb -v -d 045e:001b            # verbose descriptor dump
```

### Find the hidraw node
```bash
ls /dev/hidraw*
udevadm info /dev/hidraw0        # shows VID/PID, parent USB info
```

### Dump raw reports (before running the driver)
```bash
# While the driver is NOT running:
sudo hexdump -C /dev/hidraw0
# Wiggle wheel and pedals; watch changing bytes
```

### hid-recorder (best tool for parser development)
```bash
sudo apt install hid-tools          # or: pip install hid-tools
sudo hid-recorder /dev/hidraw0      # records descriptor + reports
# Replay with: hid-replay <recording>
```

### Use --debug mode
Run the driver with `--debug` and move every control to see which bytes change:
```
[RAW  8 bytes] 01 00 02 ff 00 01 00 00
  steer=  -128  throttle=255  brake=  0  buttons=0b00000001 0b00000000
```

---

## Updating the parser

All byte-offset logic is in **`src/parser.rs`** in the `WheelState::parse()` function.
Every assumption is marked with `⚠ ASSUMPTION`. To update:

1. Run `--debug` and capture raw bytes for all controls at min, centre, max
2. Identify which bytes/bits correspond to which controls
3. Update the byte offsets and bit masks in `parse()`
4. Re-run `cargo build` and test with `evtest`

---

## Force feedback — future work

The SideWinder FFB Wheel has a hardware force feedback motor. Full FF support
requires receiving **`EV_FF`** events from the game and converting them to
USB HID output reports sent back to the wheel.

### Why it's not implemented yet

- The HID output report format for FF commands is reverse-engineered and
  device-specific. It needs captures from Windows with a USB analyser or
  `usbmon`.
- `hidapi` supports `device.write()` for output reports, so the plumbing
  is straightforward once the report format is known.

### How to add it later

1. **Declare EV_FF support** in `uinput.rs`:
   ```rust
   Self::set_evbit(fd, EV_FF as i32)?;
   // Also declare FF_CONSTANT, FF_SPRING, FF_DAMPER, etc.
   ```

2. **Open a second thread or use epoll** to listen for `EV_FF` write events
   coming back *into* the virtual device from games (games write FF effects
   to `/dev/input/eventX`; you read them back with a second fd).

3. **Translate** each `ff_effect` struct into the wheel's proprietary HID
   output report and call `device.write(&report)`.

4. **Known limitation**: Userspace FF bridging has latency. The original
   in-kernel `hid-sidewinder` driver could achieve tighter timing. For most
   games, userspace latency (~1–5 ms round-trip) is acceptable.

### Useful references
- `linux/input.h` — `struct ff_effect`, `FF_*` constants
- `linux/uinput.h` — `UI_BEGIN_FF_UPLOAD`, `UI_END_FF_UPLOAD`
- The `evdev` crate has `UInputHandle` with FF upload support
- LibreFFB project has examples of userspace FF bridging

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| `Cannot open /dev/uinput` | Install udev rules; add user to `input` group |
| `Could not open HID device` | Wrong PID — run `--list`; try `--pid 0xXXXX` |
| Axes inverted | Edit `parser.rs`: invert axis with `255 - value` or negate |
| Axes don't reach full range | Adjust `abs_setup()` min/max in `uinput.rs` |
| Buttons wrong | Re-check bit order in `parser.rs`; dump raw bytes with `--debug` |
| Device not seen by game | Check `evtest`; game may need `/dev/input/jsX` — install `xboxdrv` or `joydev` module |
