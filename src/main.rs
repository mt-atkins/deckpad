//! deckpad: a lean Stream Deck daemon with deck, gamepad and MIDI drum pad modes.

mod config;
mod gamepad;
mod keyboard;
mod midi;
mod render;

use anyhow::{Context, Result, bail};
use config::{Config, Mode, parse_color};
use elgato_streamdeck::images::convert_image;
use elgato_streamdeck::info::Kind;
use elgato_streamdeck::{DeviceStateUpdate, StreamDeck, StreamDeckError, list_devices, new_hidapi, refresh_device_list};
use evdev::KeyCode;
use gamepad::{Gamepad, PadInput};
use hidapi::HidError;
use image::{DynamicImage, RgbImage};
use keyboard::Keyboard;
use midi::MidiOut;
use render::Renderer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static RUNNING: AtomicBool = AtomicBool::new(true);

const MODES: [Mode; 3] = [Mode::Deck, Mode::Gamepad, Mode::Drum];
const MODE_KEY_COLOR: [u8; 3] = [134, 142, 150];
const DEFAULT_COLOR: [u8; 3] = [73, 80, 87];

const USAGE: &str = "\
deckpad - Stream Deck daemon with gamepad and MIDI drum pad modes

USAGE:
    deckpad [OPTIONS]

OPTIONS:
    -m, --mode <deck|gamepad|drum>   Mode to start in (overrides the config)
    -c, --config <FILE>              Config file (default: ~/.config/deckpad/config.toml)
        --list                       List connected Stream Decks and exit
        --print-config               Print the built-in default config and exit
        --preview <FILE.png>         Render every mode's layout to an image and exit
    -h, --help                       Show this help
";

/// What a deck key does while held.
#[derive(Debug, Clone)]
enum Action {
    CycleMode,
    SetMode(Mode),
    Combo(Vec<KeyCode>),
    Command(String),
    Pad(PadInput),
    Note(u8),
    None,
}

struct Binding {
    label: String,
    color: [u8; 3],
    action: Action,
}

type Layout = HashMap<u8, Binding>;

fn color_or(color: &Option<String>, default: [u8; 3]) -> Result<[u8; 3]> {
    color.as_deref().map(parse_color).unwrap_or(Ok(default))
}

/// Turns the config into one key layout per mode, failing early on bad entries.
fn build_layout(cfg: &Config, mode: Mode) -> Result<Layout> {
    let mut layout = Layout::new();
    match mode {
        Mode::Deck => {
            for k in &cfg.deck.keys {
                let action = if let Some(combo) = &k.keys {
                    Action::Combo(keyboard::parse_combo(combo).with_context(|| format!("deck key {}", k.key))?)
                } else if let Some(cmd) = &k.command {
                    Action::Command(cmd.clone())
                } else if let Some(m) = k.mode {
                    Action::SetMode(m)
                } else {
                    Action::None
                };
                layout.insert(k.key, Binding { label: k.label.clone(), color: color_or(&k.color, DEFAULT_COLOR)?, action });
            }
        }
        Mode::Gamepad => {
            for k in &cfg.gamepad.keys {
                let input = PadInput::parse(&k.input).with_context(|| format!("gamepad key {}", k.key))?;
                layout.insert(
                    k.key,
                    Binding {
                        label: k.label.clone().unwrap_or_else(|| input.default_label()),
                        color: color_or(&k.color, input.default_color())?,
                        action: Action::Pad(input),
                    },
                );
            }
        }
        Mode::Drum => {
            for p in &cfg.drum.pads {
                layout.insert(p.key, Binding { label: p.label.clone(), color: color_or(&p.color, DEFAULT_COLOR)?, action: Action::Note(p.note) });
            }
        }
    }
    if let Some(key) = cfg.mode_key {
        layout.insert(key, Binding { label: format!("MODE\n{}", mode.name()), color: MODE_KEY_COLOR, action: Action::CycleMode });
    }
    Ok(layout)
}

/// Pre-encoded images for one key, ready to send to the device.
struct Face {
    normal: Vec<u8>,
    pressed: Vec<u8>,
}

struct App {
    cfg: Config,
    renderer: Renderer,
    layouts: HashMap<u8, Layout>,
    mode: Mode,
    faces: HashMap<u8, Face>,
    held: HashMap<u8, Action>,
    keyboard: Option<Keyboard>,
    gamepad: Option<Gamepad>,
    midi: Option<MidiOut>,
}

fn mode_index(mode: Mode) -> u8 {
    MODES.iter().position(|m| *m == mode).unwrap() as u8
}

fn log_err(what: &str, result: Result<()>) {
    if let Err(e) = result {
        eprintln!("deckpad: {what}: {e:#}");
    }
}

impl App {
    fn new(cfg: Config, mode: Mode) -> Result<App> {
        let mut layouts = HashMap::new();
        for m in MODES {
            layouts.insert(mode_index(m), build_layout(&cfg, m)?);
        }
        let renderer = Renderer::new(cfg.font.as_deref());
        Ok(App { cfg, renderer, layouts, mode, faces: HashMap::new(), held: HashMap::new(), keyboard: None, gamepad: None, midi: None })
    }

    fn layout(&self) -> &Layout {
        &self.layouts[&mode_index(self.mode)]
    }

    /// Creates the virtual device the current mode needs, the first time it is entered.
    /// They then stay alive so e.g. a game does not see its controller unplugged
    /// when you hop to another mode and back.
    fn prepare_mode(&mut self) {
        match self.mode {
            Mode::Deck if self.keyboard.is_none() => {
                let mut used: Vec<KeyCode> = Vec::new();
                for binding in self.layout().values() {
                    if let Action::Combo(keys) = &binding.action {
                        used.extend(keys);
                    }
                }
                if !used.is_empty() {
                    match Keyboard::new(&used) {
                        Ok(kb) => self.keyboard = Some(kb),
                        Err(e) => eprintln!("deckpad: key combos disabled: {e:#}"),
                    }
                }
            }
            Mode::Gamepad if self.gamepad.is_none() => match Gamepad::new(&self.cfg.gamepad.name) {
                Ok(pad) => {
                    println!("deckpad: virtual gamepad '{}' created", self.cfg.gamepad.name);
                    self.gamepad = Some(pad);
                }
                Err(e) => eprintln!("deckpad: gamepad mode disabled: {e:#}"),
            },
            Mode::Drum if self.midi.is_none() => match MidiOut::new(&self.cfg.drum.port_name, self.cfg.drum.connect.as_deref()) {
                Ok(midi) => {
                    println!("deckpad: MIDI port '{}' created (connect a synth or DAW to it)", self.cfg.drum.port_name);
                    self.midi = Some(midi);
                }
                Err(e) => eprintln!("deckpad: drum mode disabled: {e:#}"),
            },
            _ => {}
        }
    }

    fn enter_mode(&mut self, deck: &StreamDeck, mode: Mode) -> Result<()> {
        self.release_all();
        self.mode = mode;
        self.prepare_mode();

        let kind = deck.kind();
        let size = kind.key_image_format().size.0 as u32;
        self.faces.clear();
        for key in 0..kind.key_count() {
            match self.layout().get(&key) {
                Some(b) => {
                    let encode = |pressed| convert_image(kind, DynamicImage::ImageRgb8(self.renderer.face(size, &b.label, b.color, pressed)));
                    let face = Face { normal: encode(false)?, pressed: encode(true)? };
                    deck.write_image(key, &face.normal)?;
                    self.faces.insert(key, face);
                }
                None => deck.clear_button_image(key)?,
            }
        }
        deck.flush()?;
        println!("deckpad: {} mode", mode.id());
        Ok(())
    }

    fn show(&self, deck: &StreamDeck, key: u8, pressed: bool) -> Result<()> {
        if let Some(face) = self.faces.get(&key) {
            deck.write_image(key, if pressed { &face.pressed } else { &face.normal })?;
            deck.flush()?;
        }
        Ok(())
    }

    fn key_down(&mut self, deck: &StreamDeck, key: u8) -> Result<()> {
        let Some(action) = self.layout().get(&key).map(|b| b.action.clone()) else {
            return Ok(());
        };
        match &action {
            Action::CycleMode => return self.enter_mode(deck, self.mode.next()),
            Action::SetMode(mode) => return self.enter_mode(deck, *mode),
            _ => {}
        }
        // Act first, repaint second: the output event is what needs low latency.
        self.apply(&action, true);
        self.held.insert(key, action);
        self.show(deck, key, true)
    }

    fn key_up(&mut self, deck: &StreamDeck, key: u8) -> Result<()> {
        // Keys pressed before a mode switch are no longer in `held`; ignore their release.
        if let Some(action) = self.held.remove(&key) {
            self.apply(&action, false);
            self.show(deck, key, false)?;
        }
        Ok(())
    }

    fn apply(&mut self, action: &Action, pressed: bool) {
        match action {
            Action::Combo(keys) => {
                if let Some(kb) = &mut self.keyboard {
                    log_err("key combo", if pressed { kb.press(keys) } else { kb.release(keys) });
                }
            }
            Action::Command(cmd) if pressed => log_err("command", spawn_command(cmd)),
            Action::Pad(input) => {
                if let Some(pad) = &mut self.gamepad {
                    log_err("gamepad", pad.set(*input, pressed));
                }
            }
            Action::Note(note) => {
                if let Some(midi) = &self.midi {
                    log_err("midi", midi.note(pressed, self.cfg.drum.channel - 1, *note, self.cfg.drum.velocity));
                }
            }
            _ => {}
        }
    }

    /// Releases everything still held, so no button or note sticks.
    fn release_all(&mut self) {
        for (_, action) in std::mem::take(&mut self.held) {
            self.apply(&action, false);
        }
    }

    /// Runs until the device fails (returns Err) or shutdown is requested.
    fn session(&mut self, deck: StreamDeck) -> Result<()> {
        deck.set_brightness(self.cfg.brightness.min(100))?;
        self.enter_mode(&deck, self.mode)?;
        let deck = Arc::new(deck);
        let reader = deck.get_reader();
        while RUNNING.load(Ordering::Relaxed) {
            let updates = match reader.read(Some(Duration::from_millis(100))) {
                Ok(updates) => updates,
                // A signal (Ctrl-C) interrupts the blocking read; that is not a device
                // fault. Loop again so the shutdown flag is seen once the handler sets it.
                Err(e) if is_interrupted(&e) || !RUNNING.load(Ordering::Relaxed) => continue,
                Err(e) => return Err(e.into()),
            };
            for update in updates {
                match update {
                    DeviceStateUpdate::ButtonDown(key) => self.key_down(&deck, key)?,
                    DeviceStateUpdate::ButtonUp(key) => self.key_up(&deck, key)?,
                    _ => {}
                }
            }
        }
        deck.reset()?;
        Ok(())
    }
}

fn spawn_command(cmd: &str) -> Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("running '{cmd}'"))?;
    // Reap the child in the background so it never lingers as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Renders all three layouts (as they look on a 5x3 deck) into one image.
fn preview(app: &App, path: &PathBuf) -> Result<()> {
    let (cols, rows, size, gap) = (5u32, 3u32, 144u32, 24u32);
    let block_h = rows * (size + gap) + gap * 2;
    let mut sheet = RgbImage::from_pixel(cols * (size + gap) + gap, block_h * MODES.len() as u32, image::Rgb([24, 24, 28]));
    for (i, mode) in MODES.iter().enumerate() {
        let layout = &app.layouts[&mode_index(*mode)];
        for key in 0..(cols * rows) {
            let Some(b) = layout.get(&(key as u8)) else { continue };
            let face = app.renderer.face(size, &b.label, b.color, false);
            let x = gap + (key % cols) * (size + gap);
            let y = block_h * i as u32 + gap * 2 + (key / cols) * (size + gap);
            image::imageops::replace(&mut sheet, &face, x as i64, y as i64);
        }
    }
    sheet.save(path).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {} (top to bottom: deck, gamepad, drum)", path.display());
    Ok(())
}

fn sleep_while_running(total: Duration) {
    let step = Duration::from_millis(100);
    let mut waited = Duration::ZERO;
    while waited < total && RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(step);
        waited += step;
    }
}

fn run() -> Result<()> {
    let mut mode_arg: Option<Mode> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut preview_path: Option<PathBuf> = None;
    let mut list = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-m" | "--mode" => mode_arg = Some(Mode::parse(&value("--mode")?)?),
            "-c" | "--config" => config_path = Some(PathBuf::from(value("--config")?)),
            "--preview" => preview_path = Some(PathBuf::from(value("--preview")?)),
            "--list" => list = true,
            "--print-config" => {
                print!("{}", config::DEFAULT_CONFIG);
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument '{other}' (try --help)"),
        }
    }

    if list {
        let hid = new_hidapi().context("initialising HID")?;
        let devices = list_devices(&hid);
        if devices.is_empty() {
            println!("no Stream Deck found");
        }
        for (kind, serial) in devices {
            println!("{kind:?}  serial {serial}  ({} keys)", kind.key_count());
        }
        return Ok(());
    }

    let (cfg, source) = Config::load(config_path.as_deref())?;
    let mode = mode_arg.unwrap_or(cfg.start_mode);
    let mut app = App::new(cfg, mode)?;

    if let Some(path) = preview_path {
        return preview(&app, &path);
    }

    println!("deckpad: config from {source}");
    ctrlc::set_handler(|| RUNNING.store(false, Ordering::Relaxed)).context("installing signal handler")?;

    let mut hid = new_hidapi().context("initialising HID")?;
    let mut waiting = false;
    while RUNNING.load(Ordering::Relaxed) {
        refresh_device_list(&mut hid).context("scanning for devices")?;
        let Some((kind, serial)) = list_devices(&hid).into_iter().next() else {
            if !waiting {
                println!("deckpad: waiting for a Stream Deck...");
                waiting = true;
            }
            sleep_while_running(Duration::from_secs(1));
            continue;
        };
        waiting = false;
        match StreamDeck::connect(&hid, kind, &serial) {
            Ok(deck) => {
                println!("deckpad: connected to {} ({serial})", describe(kind));
                let result = app.session(deck);
                app.release_all();
                if let Err(e) = result {
                    eprintln!("deckpad: device error: {e:#}; reconnecting");
                    sleep_while_running(Duration::from_secs(1));
                }
            }
            Err(e) => {
                eprintln!("deckpad: cannot open {} ({e}); check the udev rule in the README. Retrying...", describe(kind));
                sleep_while_running(Duration::from_secs(3));
            }
        }
    }
    Ok(())
}

fn is_interrupted(err: &StreamDeckError) -> bool {
    matches!(err, StreamDeckError::HidError(HidError::IoError { error }) if error.kind() == std::io::ErrorKind::Interrupted)
}

fn describe(kind: Kind) -> String {
    format!("Stream Deck {kind:?}")
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("deckpad: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_builds_every_layout() {
        let cfg: Config = toml::from_str(config::DEFAULT_CONFIG).unwrap();
        for mode in MODES {
            let layout = build_layout(&cfg, mode).unwrap();
            assert!(matches!(layout[&cfg.mode_key.unwrap()].action, Action::CycleMode));
            assert!(layout.len() > 10, "{mode:?} layout looks empty");
        }
    }

    #[test]
    fn modes_cycle_through_all_three() {
        assert_eq!(Mode::Deck.next().next().next(), Mode::Deck);
    }
}
