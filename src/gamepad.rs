//! Gamepad mode: a virtual Xbox-style controller created through uinput.

use anyhow::{Context, Result, bail};
use evdev::uinput::VirtualDevice;
use evdev::{AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode, UinputAbsSetup};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Something a deck key can be bound to on the virtual controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadInput {
    Button(KeyCode),
    Dpad(Dir),
    Stick(Side, Dir),
    Trigger(Side),
}

// Same button codes the kernel's xpad driver reports, so games and Steam
// map the virtual pad like a real Xbox controller.
const BUTTONS: &[(&str, KeyCode, &str, [u8; 3])] = &[
    ("a", KeyCode::BTN_SOUTH, "A", [47, 158, 68]),
    ("b", KeyCode::BTN_EAST, "B", [201, 42, 42]),
    ("x", KeyCode::BTN_NORTH, "X", [25, 113, 194]),
    ("y", KeyCode::BTN_WEST, "Y", [232, 184, 0]),
    ("lb", KeyCode::BTN_TL, "LB", [73, 80, 87]),
    ("rb", KeyCode::BTN_TR, "RB", [73, 80, 87]),
    ("select", KeyCode::BTN_SELECT, "SEL", [52, 58, 64]),
    ("start", KeyCode::BTN_START, "START", [52, 58, 64]),
    ("guide", KeyCode::BTN_MODE, "GUIDE", [52, 58, 64]),
    ("l3", KeyCode::BTN_THUMBL, "L3", [73, 80, 87]),
    ("r3", KeyCode::BTN_THUMBR, "R3", [73, 80, 87]),
];

impl PadInput {
    pub fn parse(s: &str) -> Result<PadInput> {
        let name = s.trim().to_ascii_lowercase();
        if let Some((_, code, _, _)) = BUTTONS.iter().find(|(n, ..)| *n == name) {
            return Ok(PadInput::Button(*code));
        }
        let dir = |d: &str| -> Result<Dir> {
            Ok(match d {
                "up" => Dir::Up,
                "down" => Dir::Down,
                "left" => Dir::Left,
                "right" => Dir::Right,
                _ => bail!("unknown gamepad input '{s}'"),
            })
        };
        Ok(match name.as_str() {
            "lt" => PadInput::Trigger(Side::Left),
            "rt" => PadInput::Trigger(Side::Right),
            n => match n.split_once('_') {
                Some(("dpad", d)) => PadInput::Dpad(dir(d)?),
                Some(("ls", d)) => PadInput::Stick(Side::Left, dir(d)?),
                Some(("rs", d)) => PadInput::Stick(Side::Right, dir(d)?),
                _ => bail!("unknown gamepad input '{s}'"),
            },
        })
    }

    pub fn default_label(self) -> String {
        let arrow = |d: Dir| match d {
            Dir::Up => "↑",
            Dir::Down => "↓",
            Dir::Left => "←",
            Dir::Right => "→",
        };
        match self {
            PadInput::Button(code) => BUTTONS.iter().find(|b| b.1 == code).map(|b| b.2).unwrap_or("?").to_string(),
            PadInput::Dpad(d) => arrow(d).to_string(),
            PadInput::Stick(Side::Left, d) => format!("LS\n{}", arrow(d)),
            PadInput::Stick(Side::Right, d) => format!("RS\n{}", arrow(d)),
            PadInput::Trigger(Side::Left) => "LT".to_string(),
            PadInput::Trigger(Side::Right) => "RT".to_string(),
        }
    }

    pub fn default_color(self) -> [u8; 3] {
        match self {
            PadInput::Button(code) => BUTTONS.iter().find(|b| b.1 == code).map(|b| b.3).unwrap_or([73, 80, 87]),
            PadInput::Dpad(_) => [11, 114, 133],
            PadInput::Stick(..) => [95, 61, 196],
            PadInput::Trigger(_) => [134, 46, 156],
        }
    }
}

const STICK_MAX: i32 = 32767;
const TRIGGER_MAX: i32 = 255;

pub struct Gamepad {
    dev: VirtualDevice,
    held: Vec<PadInput>,
}

impl Gamepad {
    pub fn new(name: &str) -> Result<Gamepad> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for (_, code, _, _) in BUTTONS {
            keys.insert(*code);
        }
        let stick = AbsInfo::new(0, -32768, STICK_MAX, 16, 128, 0);
        let trigger = AbsInfo::new(0, 0, TRIGGER_MAX, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let axis = |code, info| UinputAbsSetup::new(code, info);

        let dev = VirtualDevice::builder()
            .context("opening /dev/uinput (is your user allowed to use it? see README)")?
            .name(name)
            // Xbox 360 pad IDs: recognised out of the box by SDL, Steam and most games.
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&keys)?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_X, stick))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Y, stick))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_RX, stick))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_RY, stick))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_Z, trigger))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_RZ, trigger))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_HAT0X, hat))?
            .with_absolute_axis(&axis(AbsoluteAxisCode::ABS_HAT0Y, hat))?
            .build()
            .context("creating virtual gamepad")?;
        Ok(Gamepad { dev, held: Vec::new() })
    }

    pub fn set(&mut self, input: PadInput, pressed: bool) -> Result<()> {
        self.held.retain(|h| *h != input);
        if pressed {
            self.held.push(input);
        }
        let event = match input {
            PadInput::Button(code) => InputEvent::new(EventType::KEY.0, code.0, pressed as i32),
            PadInput::Trigger(side) => {
                let code = match side {
                    Side::Left => AbsoluteAxisCode::ABS_Z,
                    Side::Right => AbsoluteAxisCode::ABS_RZ,
                };
                InputEvent::new(EventType::ABSOLUTE.0, code.0, if pressed { TRIGGER_MAX } else { 0 })
            }
            // Axis values come from everything currently held, so opposite
            // directions cancel and releasing one restores the other.
            PadInput::Dpad(dir) => {
                let (code, neg, pos) = match dir {
                    Dir::Left | Dir::Right => (AbsoluteAxisCode::ABS_HAT0X, Dir::Left, Dir::Right),
                    Dir::Up | Dir::Down => (AbsoluteAxisCode::ABS_HAT0Y, Dir::Up, Dir::Down),
                };
                let value = self.is_held(PadInput::Dpad(pos)) as i32 - self.is_held(PadInput::Dpad(neg)) as i32;
                InputEvent::new(EventType::ABSOLUTE.0, code.0, value)
            }
            PadInput::Stick(side, dir) => {
                let horizontal = matches!(dir, Dir::Left | Dir::Right);
                let code = match (side, horizontal) {
                    (Side::Left, true) => AbsoluteAxisCode::ABS_X,
                    (Side::Left, false) => AbsoluteAxisCode::ABS_Y,
                    (Side::Right, true) => AbsoluteAxisCode::ABS_RX,
                    (Side::Right, false) => AbsoluteAxisCode::ABS_RY,
                };
                let (neg, pos) = if horizontal { (Dir::Left, Dir::Right) } else { (Dir::Up, Dir::Down) };
                let value = self.is_held(PadInput::Stick(side, pos)) as i32 - self.is_held(PadInput::Stick(side, neg)) as i32;
                InputEvent::new(EventType::ABSOLUTE.0, code.0, value * STICK_MAX)
            }
        };
        self.dev.emit(&[event]).context("sending gamepad event")
    }

    fn is_held(&self, input: PadInput) -> bool {
        self.held.contains(&input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_inputs() {
        assert_eq!(PadInput::parse("A").unwrap(), PadInput::Button(KeyCode::BTN_SOUTH));
        assert_eq!(PadInput::parse("dpad_up").unwrap(), PadInput::Dpad(Dir::Up));
        assert_eq!(PadInput::parse("rs_left").unwrap(), PadInput::Stick(Side::Right, Dir::Left));
        assert_eq!(PadInput::parse("lt").unwrap(), PadInput::Trigger(Side::Left));
        assert!(PadInput::parse("dpad_sideways").is_err());
        assert!(PadInput::parse("turbo").is_err());
    }

    /// Creates a real virtual controller and reads its events back.
    /// Needs access to /dev/uinput: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn virtual_pad_emits_events() {
        let mut pad = Gamepad::new("deckpad test pad").unwrap();
        let node = pad.dev.enumerate_dev_nodes_blocking().unwrap().next().unwrap().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300)); // let udev set permissions
        let mut reader = evdev::Device::open(&node).unwrap();

        pad.set(PadInput::Button(KeyCode::BTN_SOUTH), true).unwrap();
        pad.set(PadInput::Dpad(Dir::Left), true).unwrap();
        pad.set(PadInput::Dpad(Dir::Right), true).unwrap(); // opposite directions cancel
        pad.set(PadInput::Dpad(Dir::Left), false).unwrap();
        pad.set(PadInput::Stick(Side::Left, Dir::Up), true).unwrap();
        pad.set(PadInput::Trigger(Side::Right), true).unwrap();

        let got: Vec<(u16, u16, i32)> = reader
            .fetch_events()
            .unwrap()
            .filter(|e| e.event_type() != EventType::SYNCHRONIZATION)
            .map(|e| (e.event_type().0, e.code(), e.value()))
            .collect();
        let (key, abs) = (EventType::KEY.0, EventType::ABSOLUTE.0);
        assert_eq!(
            got,
            vec![
                (key, KeyCode::BTN_SOUTH.0, 1),
                (abs, AbsoluteAxisCode::ABS_HAT0X.0, -1),
                (abs, AbsoluteAxisCode::ABS_HAT0X.0, 0),
                (abs, AbsoluteAxisCode::ABS_HAT0X.0, 1),
                (abs, AbsoluteAxisCode::ABS_Y.0, -STICK_MAX),
                (abs, AbsoluteAxisCode::ABS_RZ.0, TRIGGER_MAX),
            ]
        );
    }
}
