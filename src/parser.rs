// src/parser.rs
//
// Raw HID report parser for the Microsoft SideWinder Force Feedback Wheel.
//
// ── Development workflow ──────────────────────────────────────────────────────
//
//  1. ENUMERATE: Run `sidewinder-wheel --list` to confirm VID/PID.
//  2. DUMP RAW:  Run `sidewinder-wheel --debug` and turn the wheel / press
//                pedals. Watch the hex output to identify which bytes change.
//  3. IDENTIFY LAYOUT: Look for:
//       - bytes that sweep the full 0..FF or 00..FF range (axes)
//       - bytes where individual bits toggle on button presses
//       - constant bytes (report ID, padding, device status)
//  4. MAP:  Fill in the `parse()` function below with the real byte offsets.
//  5. EXPOSE: The main loop feeds WheelState into the uinput layer.
//
// ── Known information about SideWinder FFB Wheel ─────────────────────────────
//
// The SideWinder Force Feedback Wheel is a USB HID device.
// Its VID is 0x045e (Microsoft), PID is typically 0x001b.
// Report length is typically 8 bytes for the main input report.
//
// ASSUMED layout below (⚠ VERIFY WITH RAW DUMPS — see --debug flag):
//
//   Byte 0    : Report ID (often 0x01 for input)        [ASSUMPTION]
//   Bytes 1-2 : Steering wheel position, 10-bit or 16-bit, little-endian
//               Centered ~= 0x200 (10-bit) or 0x0000 (signed 16-bit) [ASSUMPTION]
//   Byte 3    : Throttle / accelerator pedal, 0x00..0xFF [ASSUMPTION]
//   Byte 4    : Brake pedal, 0x00..0xFF                 [ASSUMPTION]
//   Byte 5    : Button bitmask, bits 0-7                [ASSUMPTION]
//   Byte 6    : More buttons or hat switch              [ASSUMPTION]
//   Byte 7    : Status / padding                        [ASSUMPTION]
//
// All ASSUMPTION markers must be confirmed against real captures.
// Use `hid-recorder`, `hexdump -C /dev/hidraw0`, or --debug mode.

/// Parsed state of the wheel at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct WheelState {
    /// Steering wheel position, normalised to -32768..32767.
    /// Negative = left, positive = right.
    pub steering: i16,

    /// Accelerator/throttle pedal, 0..255 (0 = released, 255 = floored).
    pub throttle: u8,

    /// Brake pedal, 0..255 (0 = released, 255 = fully depressed).
    pub brake: u8,

    /// Up to 8 buttons, packed as a bitmask.
    /// Bit 0 = button 1 (trigger), bit 1 = button 2, …
    pub buttons: u8,

    /// Second button byte for wheels with more than 8 buttons.
    pub buttons2: u8,
}

impl Default for WheelState {
    fn default() -> Self {
        Self {
            steering: 0,
            throttle: 0,
            brake: 0,
            buttons: 0,
            buttons2: 0,
        }
    }
}

impl WheelState {
    /// Parse a raw HID input report into a WheelState.
    ///
    /// `report` is the raw byte slice returned by hidapi (may or may not include
    /// a leading report-ID byte depending on how hidapi opens the device).
    ///
    /// Returns `None` if the report is too short or has an unexpected ID.
    ///
    /// ⚠ ALL byte-offset logic here is ASSUMED. Verify with --debug output.
    pub fn parse(report: &[u8]) -> Option<Self> {
        // Minimum length guard — adjust once real report length is known.
        if report.len() < 6 {
            log::warn!(
                "Report too short: {} bytes (expected ≥ 6). \
                 Check --debug output to see the real report length.",
                report.len()
            );
            return None;
        }

        // ── Byte 0: Report ID ─────────────────────────────────────────────
        // hidapi with report IDs prepends the ID as byte 0.
        // If the device has only one report, hidapi may or may not include it.
        //
        // ⚠ ASSUMPTION: Report ID = 0x01 for the main input report.
        // If dumps show a different ID at byte 0, update this check.
        // If byte 0 is NOT a report ID (some devices omit it), remove this check
        // and shift all offsets down by 1.
        let id = report[0];
        if id != 0x01 {
            log::debug!("Ignoring report with ID 0x{:02x}", id);
            return None;
        }

        // ── Bytes 1-2: Steering wheel ─────────────────────────────────────
        //
        // ⚠ ASSUMPTION: 10-bit unsigned value, little-endian, in the lower
        //   10 bits of a 16-bit word.  Centre ≈ 0x200 (512).
        //   Range: 0x000 (full left) .. 0x3FF (full right).
        //
        // We normalise to i16: (raw - 512) * 64  → -32768..32767.
        //
        // Alternative to try if this looks wrong:
        //   - Signed 16-bit: i16::from_le_bytes([report[1], report[2]])
        //   - 8-bit only: report[1] as i16 * 256
        let raw_steer = u16::from_le_bytes([report[1], report[2]]) & 0x3FF;
        let steering = ((raw_steer as i32 - 512) * 64).clamp(-32768, 32767) as i16;

        // ── Byte 3: Throttle ──────────────────────────────────────────────
        //
        // ⚠ ASSUMPTION: 8-bit unsigned, 0x00 = released, 0xFF = floored.
        // Some pedal axes are inverted (0xFF at rest). If so, add: 255 - report[3]
        let throttle = report[3];

        // ── Byte 4: Brake ─────────────────────────────────────────────────
        //
        // ⚠ ASSUMPTION: 8-bit unsigned, same convention as throttle.
        let brake = report[4];

        // ── Byte 5: Buttons ───────────────────────────────────────────────
        //
        // ⚠ ASSUMPTION: Each bit = one button.
        //   Bit 0 = trigger (main fire button)
        //   Bit 1 = thumb / secondary
        //   Bit 2..7 = additional buttons
        let buttons = report[5];

        // ── Byte 6: More buttons / hat ────────────────────────────────────
        //
        // ⚠ ASSUMPTION: Second button byte or hat-switch nibble.
        // For now, treat as raw button byte. Hat-switch decoding can be added later.
        let buttons2 = if report.len() > 6 { report[6] } else { 0 };

        Some(Self {
            steering,
            throttle,
            brake,
            buttons,
            buttons2,
        })
    }

    /// Return true if button N (0-indexed) is pressed.
    pub fn button(&self, n: u8) -> bool {
        if n < 8 {
            (self.buttons >> n) & 1 == 1
        } else if n < 16 {
            (self.buttons2 >> (n - 8)) & 1 == 1
        } else {
            false
        }
    }
}

/// Print a raw report as annotated hex — used in --debug mode.
pub fn dump_report(report: &[u8]) {
    let hex: Vec<String> = report.iter().map(|b| format!("{:02x}", b)).collect();
    println!(
        "[RAW {:2} bytes] {}",
        report.len(),
        hex.join(" ")
    );

    // Try to parse and show the interpreted values alongside raw bytes.
    if let Some(state) = WheelState::parse(report) {
        println!(
            "  steer={:6}  throttle={:3}  brake={:3}  buttons=0b{:08b} 0b{:08b}",
            state.steering, state.throttle, state.brake, state.buttons, state.buttons2
        );
    } else {
        println!("  (could not parse this report — check byte layout assumptions)");
    }
}
