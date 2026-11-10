//! Deck mode key combos, typed through a virtual uinput keyboard.

use anyhow::{Context, Result, anyhow};
use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, EventType, InputEvent, KeyCode};
use std::str::FromStr;

/// Parses "ctrl+shift+t" or "KEY_VOLUMEUP" into key codes, modifiers first.
pub fn parse_combo(combo: &str) -> Result<Vec<KeyCode>> {
    combo
        .split('+')
        .map(|token| {
            let upper = token.trim().to_ascii_uppercase();
            let name = match upper.as_str() {
                "CTRL" | "CONTROL" => "KEY_LEFTCTRL".to_string(),
                "SHIFT" => "KEY_LEFTSHIFT".to_string(),
                "ALT" => "KEY_LEFTALT".to_string(),
                "SUPER" | "META" | "WIN" => "KEY_LEFTMETA".to_string(),
                n if n.starts_with("KEY_") || n.starts_with("BTN_") => n.to_string(),
                n => format!("KEY_{n}"),
            };
            KeyCode::from_str(&name).map_err(|_| anyhow!("unknown key '{}' in combo '{combo}'", token.trim()))
        })
        .collect()
}

pub struct Keyboard {
    dev: VirtualDevice,
}

impl Keyboard {
    /// `used` lists every key the config can press; uinput needs them declared up front.
    pub fn new(used: &[KeyCode]) -> Result<Keyboard> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in used {
            keys.insert(*code);
        }
        let dev = VirtualDevice::builder()
            .context("opening /dev/uinput (is your user allowed to use it? see README)")?
            .name("deckpad Virtual Keyboard")
            .with_keys(&keys)?
            .build()
            .context("creating virtual keyboard")?;
        Ok(Keyboard { dev })
    }

    pub fn press(&mut self, combo: &[KeyCode]) -> Result<()> {
        let events: Vec<_> = combo.iter().map(|k| InputEvent::new(EventType::KEY.0, k.0, 1)).collect();
        self.dev.emit(&events).context("sending key press")
    }

    pub fn release(&mut self, combo: &[KeyCode]) -> Result<()> {
        let events: Vec<_> = combo.iter().rev().map(|k| InputEvent::new(EventType::KEY.0, k.0, 0)).collect();
        self.dev.emit(&events).context("sending key release")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combos() {
        assert_eq!(parse_combo("ctrl+shift+t").unwrap(), vec![KeyCode::KEY_LEFTCTRL, KeyCode::KEY_LEFTSHIFT, KeyCode::KEY_T]);
        assert_eq!(parse_combo("KEY_VOLUMEUP").unwrap(), vec![KeyCode::KEY_VOLUMEUP]);
        assert_eq!(parse_combo("playpause").unwrap(), vec![KeyCode::KEY_PLAYPAUSE]);
        assert!(parse_combo("ctrl+nosuchkey").is_err());
    }
}
