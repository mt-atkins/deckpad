//! Configuration file model. A user config replaces the embedded defaults.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = include_str!("../config.default.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Deck,
    Gamepad,
    Drum,
}

impl Mode {
    pub fn next(self) -> Mode {
        match self {
            Mode::Deck => Mode::Gamepad,
            Mode::Gamepad => Mode::Drum,
            Mode::Drum => Mode::Deck,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Deck => "DECK",
            Mode::Gamepad => "PAD",
            Mode::Drum => "DRUM",
        }
    }

    /// Name used on the command line, in the config and in logs.
    pub fn id(self) -> &'static str {
        match self {
            Mode::Deck => "deck",
            Mode::Gamepad => "gamepad",
            Mode::Drum => "drum",
        }
    }

    pub fn parse(s: &str) -> Result<Mode> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "deck" => Mode::Deck,
            "gamepad" | "pad" => Mode::Gamepad,
            "drum" | "drumpad" | "midi" => Mode::Drum,
            other => bail!("unknown mode '{other}' (expected deck, gamepad or drum)"),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_brightness")]
    pub brightness: u8,
    #[serde(default = "default_mode")]
    pub start_mode: Mode,
    pub mode_key: Option<u8>,
    pub font: Option<PathBuf>,
    #[serde(default)]
    pub deck: DeckCfg,
    #[serde(default)]
    pub gamepad: GamepadCfg,
    #[serde(default)]
    pub drum: DrumCfg,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckCfg {
    #[serde(default)]
    pub keys: Vec<DeckKey>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeckKey {
    pub key: u8,
    pub label: String,
    pub color: Option<String>,
    pub keys: Option<String>,
    pub command: Option<String>,
    pub mode: Option<Mode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamepadCfg {
    #[serde(default = "default_pad_name")]
    pub name: String,
    #[serde(default)]
    pub keys: Vec<PadKey>,
}

impl Default for GamepadCfg {
    fn default() -> Self {
        GamepadCfg { name: default_pad_name(), keys: Vec::new() }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PadKey {
    pub key: u8,
    pub input: String,
    pub label: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrumCfg {
    #[serde(default = "default_port_name")]
    pub port_name: String,
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default = "default_velocity")]
    pub velocity: u8,
    pub connect: Option<String>,
    #[serde(default)]
    pub pads: Vec<DrumPad>,
}

impl Default for DrumCfg {
    fn default() -> Self {
        DrumCfg {
            port_name: default_port_name(),
            channel: default_channel(),
            velocity: default_velocity(),
            connect: None,
            pads: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrumPad {
    pub key: u8,
    pub note: u8,
    pub label: String,
    pub color: Option<String>,
}

fn default_brightness() -> u8 {
    60
}
fn default_mode() -> Mode {
    Mode::Deck
}
fn default_pad_name() -> String {
    "deckpad Virtual Gamepad".into()
}
fn default_port_name() -> String {
    "deckpad".into()
}
fn default_channel() -> u8 {
    10
}
fn default_velocity() -> u8 {
    110
}

impl Config {
    /// Loads `explicit` if given, else the user config if present, else the defaults.
    pub fn load(explicit: Option<&Path>) -> Result<(Config, String)> {
        let (text, source) = match explicit {
            Some(p) => (
                std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
                p.display().to_string(),
            ),
            None => match user_config_path().filter(|p| p.exists()) {
                Some(p) => (
                    std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?,
                    p.display().to_string(),
                ),
                None => (DEFAULT_CONFIG.to_string(), "built-in defaults".to_string()),
            },
        };
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing config ({source})"))?;
        cfg.validate()?;
        Ok((cfg, source))
    }

    fn validate(&self) -> Result<()> {
        if !(1..=16).contains(&self.drum.channel) {
            bail!("drum.channel must be 1-16");
        }
        if !(1..=127).contains(&self.drum.velocity) {
            bail!("drum.velocity must be 1-127");
        }
        for pad in &self.drum.pads {
            if pad.note > 127 {
                bail!("drum pad on key {}: note must be 0-127", pad.key);
            }
        }
        for k in &self.deck.keys {
            let actions = k.keys.is_some() as u8 + k.command.is_some() as u8 + k.mode.is_some() as u8;
            if actions > 1 {
                bail!("deck key {}: set only one of keys, command, mode", k.key);
            }
        }
        Ok(())
    }
}

pub fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("deckpad").join("config.toml"))
}

/// Parses "#rrggbb" (or "rrggbb").
pub fn parse_color(s: &str) -> Result<[u8; 3]> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.is_ascii() {
        bail!("bad color '{s}' (expected #rrggbb)");
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).with_context(|| format!("bad color '{s}'"));
    Ok([byte(0)?, byte(2)?, byte(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#ff8000").unwrap(), [255, 128, 0]);
        assert_eq!(parse_color("0a0b0c").unwrap(), [10, 11, 12]);
        assert!(parse_color("#fff").is_err());
        assert!(parse_color("#gggggg").is_err());
    }

    #[test]
    fn rejects_two_actions_on_one_key() {
        let text = "[[deck.keys]]\nkey = 0\nlabel = \"x\"\nkeys = \"a\"\ncommand = \"true\"\n";
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.validate().is_err());
    }
}
