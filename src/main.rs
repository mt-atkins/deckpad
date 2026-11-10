//! deckpad: a lean Stream Deck daemon with deck, gamepad and MIDI drum pad modes.

mod config;
mod gamepad;
mod keyboard;
mod looper;
mod midi;
mod painter;
mod render;

use anyhow::{Context, Result, bail};
use config::{Config, LoopControl, Mode, parse_color};
use elgato_streamdeck::images::convert_image;
use elgato_streamdeck::info::Kind;
use elgato_streamdeck::{DeviceStateReader, DeviceStateUpdate, StreamDeck, StreamDeckError, list_devices, new_hidapi, refresh_device_list};
use evdev::KeyCode;
use gamepad::{Gamepad, PadInput};
use hidapi::HidError;
use image::{DynamicImage, RgbImage};
use keyboard::Keyboard;
use looper::{Click, LoopNote, Looper};
use midi::MidiOut;
use painter::Painter;
use render::Renderer;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
    /// Note-ons for one pad (a single note or a chord).
    Notes(Vec<LoopNote>),
    Loop(LoopControl),
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
                let action = match p.action {
                    Some(control) => Action::Loop(control),
                    None => {
                        let channel = p.channel.unwrap_or(cfg.drum.channel) - 1;
                        let velocity = p.velocity.unwrap_or(cfg.drum.velocity);
                        let notes = p.note.into_iter().chain(p.notes.iter().flatten().copied());
                        Action::Notes(notes.map(|note| LoopNote { on: true, channel, note, velocity, key: Some(p.key) }).collect())
                    }
                };
                layout.insert(p.key, Binding { label: p.label.clone(), color: color_or(&p.color, DEFAULT_COLOR)?, action });
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
    normal: Arc<Vec<u8>>,
    pressed: Arc<Vec<u8>>,
}

struct App {
    cfg: Config,
    renderer: Renderer,
    layouts: HashMap<u8, Layout>,
    mode: Mode,
    /// Per mode, per key; encoded once per connection so mode switches and
    /// key flashes never render or compress anything.
    faces: HashMap<u8, HashMap<u8, Face>>,
    key_count: u8,
    painter: Option<Painter>,
    held: HashMap<u8, Action>,
    keyboard: Option<Keyboard>,
    gamepad: Option<Gamepad>,
    midi: Option<MidiOut>,
    looper: Looper,
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
        // The click is a side-stick on the drum channel.
        let click = cfg.drum.click.then_some(Click { channel: cfg.drum.channel - 1, note: 37 });
        let looper = Looper::new(cfg.drum.bpm, cfg.drum.bars, cfg.drum.quantize, click);
        Ok(App { cfg, renderer, layouts, mode, faces: HashMap::new(), key_count: 0, painter: None, held: HashMap::new(), keyboard: None, gamepad: None, midi: None, looper })
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

    /// Renders and encodes every key face of every mode for this device.
    fn build_faces(&mut self, kind: Kind) -> Result<()> {
        let size = kind.key_image_format().size.0 as u32;
        self.key_count = kind.key_count();
        self.faces.clear();
        for mode in MODES {
            let mut faces = HashMap::new();
            for (key, b) in &self.layouts[&mode_index(mode)] {
                let encode = |lit| -> Result<Arc<Vec<u8>>> {
                    let image = DynamicImage::ImageRgb8(self.renderer.face(size, &b.label, b.color, lit));
                    Ok(Arc::new(convert_image(kind, image)?))
                };
                faces.insert(*key, Face { normal: encode(false)?, pressed: encode(true)? });
            }
            self.faces.insert(mode_index(mode), faces);
        }
        Ok(())
    }

    fn enter_mode(&mut self, mode: Mode) {
        self.release_all();
        self.mode = mode;
        self.prepare_mode();
        if let (Mode::Drum, Some(midi)) = (mode, &self.midi) {
            // Sent on every entry, so a synth connected since last time gets them too.
            for p in &self.cfg.drum.programs {
                log_err("midi", midi.program(p.channel - 1, p.program));
            }
        }
        for key in 0..self.key_count {
            self.paint(key, false);
        }
        println!("deckpad: {} mode", mode.id());
    }

    /// Queues a key's lit or normal image (or a blank, for unbound keys).
    /// Never blocks: the painter thread does the USB writes.
    fn paint(&self, key: u8, lit: bool) {
        let Some(painter) = &self.painter else { return };
        match self.faces.get(&mode_index(self.mode)).and_then(|f| f.get(&key)) {
            Some(face) => painter.image(key, if lit { face.pressed.clone() } else { face.normal.clone() }),
            None => painter.clear(key),
        }
    }

    fn key_down(&mut self, key: u8) {
        let Some(action) = self.layout().get(&key).map(|b| b.action.clone()) else {
            return;
        };
        match &action {
            Action::CycleMode => return self.enter_mode(self.mode.next()),
            Action::SetMode(mode) => return self.enter_mode(*mode),
            Action::Loop(control) => return self.loop_control(key, *control),
            _ => {}
        }
        // Act first, repaint second: the output event is what needs low latency.
        self.apply(key, &action, true);
        self.held.insert(key, action);
        self.paint(key, true);
    }

    fn key_up(&mut self, key: u8) {
        // Keys pressed before a mode switch are no longer in `held`; ignore their release.
        if let Some(action) = self.held.remove(&key) {
            self.apply(key, &action, false);
            self.paint(key, false);
        }
    }

    fn apply(&mut self, key: u8, action: &Action, pressed: bool) {
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
            Action::Notes(notes) => {
                let now = Instant::now();
                if pressed {
                    self.send_midi(notes);
                    self.looper.pad_down(now, key, notes);
                } else {
                    let offs: Vec<LoopNote> = notes.iter().map(|n| LoopNote { on: false, velocity: 0, ..*n }).collect();
                    self.send_midi(&offs);
                    self.looper.pad_up(now, key);
                }
            }
            _ => {}
        }
    }

    fn send_midi(&self, notes: &[LoopNote]) {
        if let Some(midi) = &self.midi {
            for n in notes {
                log_err("midi", midi.note(n.on, n.channel, n.note, n.velocity));
            }
        }
    }

    /// Handles a REC / PLAY / CLEAR press and relights the loop keys.
    fn loop_control(&mut self, key: u8, control: LoopControl) {
        let now = Instant::now();
        let offs = match control {
            LoopControl::Rec => {
                self.looper.toggle_record(now);
                Vec::new()
            }
            LoopControl::Play => self.looper.toggle_play(now),
            LoopControl::Clear => self.looper.clear(now),
        };
        self.send_midi(&offs);
        if control == LoopControl::Clear {
            // Momentary key: light it while held like a normal pad.
            self.held.insert(key, Action::None);
            self.paint(key, true);
        }
        for n in &offs {
            if let Some(k) = n.key.filter(|k| !self.held.contains_key(k)) {
                self.paint(k, false);
            }
        }
        self.show_loop_state();
    }

    /// REC is lit while recording, PLAY while the loop runs.
    fn show_loop_state(&self) {
        if self.mode != Mode::Drum {
            return;
        }
        for (key, binding) in self.layout() {
            match binding.action {
                Action::Loop(LoopControl::Rec) => self.paint(*key, self.looper.is_recording()),
                Action::Loop(LoopControl::Play) => self.paint(*key, self.looper.is_running()),
                _ => {}
            }
        }
    }

    /// Plays whatever the loop has due, then flashes the pads it triggered.
    fn run_loop(&mut self) {
        let due = self.looper.poll(Instant::now());
        self.send_midi(&due);
        for n in &due {
            // A pad the player is holding stays lit.
            if let Some(key) = n.key.filter(|k| !self.held.contains_key(k)) {
                self.paint(key, n.on);
            }
        }
    }

    /// Releases everything still held and stops the loop, so no button or note sticks.
    fn release_all(&mut self) {
        for (key, action) in std::mem::take(&mut self.held) {
            self.apply(key, &action, false);
        }
        let offs = self.looper.stop(Instant::now());
        self.send_midi(&offs);
    }

    /// One pass of the main loop: wait for key events (no longer than until the
    /// loop's next note is due), handle them, then play what the loop has due.
    fn tick(&mut self, reader: &DeviceStateReader) -> Result<()> {
        let idle = Duration::from_millis(100);
        let timeout = self.looper.next_due(Instant::now()).map_or(idle, |d| d.min(idle));
        let updates = match reader.read(Some(timeout)) {
            Ok(updates) => updates,
            // A signal (Ctrl-C) interrupts the blocking read; that is not a device
            // fault. Return so the caller sees the shutdown flag once it is set.
            Err(e) if is_interrupted(&e) || !RUNNING.load(Ordering::Relaxed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for update in updates {
            match update {
                DeviceStateUpdate::ButtonDown(key) => self.key_down(key),
                DeviceStateUpdate::ButtonUp(key) => self.key_up(key),
                _ => {}
            }
        }
        self.run_loop();
        if self.painter.as_ref().is_some_and(|p| p.failed()) {
            bail!("writing to the deck failed");
        }
        Ok(())
    }

    /// Runs until the device fails (returns Err) or shutdown is requested.
    /// `input` is read on this thread; `output` is handed to the painter thread.
    fn session(&mut self, input: StreamDeck, output: StreamDeck) -> Result<()> {
        output.set_brightness(self.cfg.brightness.min(100))?;
        self.build_faces(input.kind())?;
        self.painter = Some(Painter::spawn(output));
        self.enter_mode(self.mode);
        let reader = Arc::new(input).get_reader();
        let mut result = Ok(());
        while RUNNING.load(Ordering::Relaxed) && result.is_ok() {
            result = self.tick(&reader);
        }
        self.release_all();
        if let Some(painter) = self.painter.take() {
            if result.is_ok() {
                painter.reset();
            }
            // Dropping the painter waits for its queue to drain.
        }
        result
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
        // Two handles to the same device: one to read keys, one to paint them.
        let handles = StreamDeck::connect(&hid, kind, &serial).and_then(|a| Ok((a, StreamDeck::connect(&hid, kind, &serial)?)));
        match handles {
            Ok((input, output)) => {
                println!("deckpad: connected to {} ({serial})", describe(kind));
                if let Err(e) = app.session(input, output) {
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

    /// Hardware-in-the-loop: records one kick on a real Stream Deck (key presses
    /// simulated in software) and checks the loop replays it into a real MIDI
    /// port, on time. Needs a deck, ALSA and aseqdump, and no other deckpad
    /// running: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn loop_replays_on_a_real_deck() {
        let mut cfg: Config = toml::from_str(config::DEFAULT_CONFIG).unwrap();
        cfg.drum.port_name = "deckpad-looptest".into();
        cfg.drum.connect = None;
        cfg.drum.click = false;
        cfg.drum.bpm = 120.0; // one bar = 2 s
        cfg.drum.bars = 1;
        let key_of = |cfg: &Config, label: &str| cfg.drum.pads.iter().find(|p| p.label == label).unwrap().key;
        let (rec, kick, clear) = (key_of(&cfg, "REC"), key_of(&cfg, "KICK"), key_of(&cfg, "CLEAR"));

        let hid = new_hidapi().unwrap();
        let (kind, serial) = list_devices(&hid).into_iter().next().expect("no Stream Deck connected");
        let reader = Arc::new(StreamDeck::connect(&hid, kind, &serial).unwrap()).get_reader();
        let mut app = App::new(cfg, Mode::Drum).unwrap();
        app.build_faces(kind).unwrap();
        app.painter = Some(Painter::spawn(StreamDeck::connect(&hid, kind, &serial).unwrap()));
        app.enter_mode(Mode::Drum);

        let mut dump = Command::new("aseqdump").args(["-p", "deckpad-looptest"]).stdout(Stdio::piped()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(400));

        let t0 = Instant::now();
        app.key_down(rec);
        assert!(app.looper.is_recording());
        // Scripted presses: (time, key, down).
        let mut script = vec![(500, kick, true), (560, kick, false), (5200, clear, true)].into_iter().peekable();
        let mut worst_late = Duration::ZERO;
        while let Some(&(at_ms, key, down)) = script.peek() {
            if t0.elapsed() >= Duration::from_millis(at_ms) {
                script.next();
                if down { app.key_down(key) } else { app.key_up(key) }
                continue;
            }
            // When the loop next owes a note; after the tick, how far past that are we?
            let now = Instant::now();
            let target = app.looper.next_due(now).map(|d| now + d);
            app.tick(&reader).unwrap();
            if let Some(late) = target.and_then(|t| Instant::now().checked_duration_since(t)) {
                worst_late = worst_late.max(late);
            }
        }
        assert!(!app.looper.is_running());
        std::thread::sleep(Duration::from_millis(300));
        dump.kill().unwrap();
        app.painter.take().unwrap().reset();
        let out = String::from_utf8(dump.wait_with_output().unwrap().stdout).unwrap();
        let count = |kind: &str| out.lines().filter(|l| l.contains(kind) && l.contains("note 36")).count();
        // One live hit, then replays at 2.5 s and 4.5 s.
        assert_eq!((count("Note on"), count("Note off")), (3, 3), "{out}");
        println!("worst loop lateness: {worst_late:?}");
        assert!(worst_late < Duration::from_millis(20), "loop ran {worst_late:?} late");
    }

    #[test]
    fn modes_cycle_through_all_three() {
        assert_eq!(Mode::Deck.next().next().next(), Mode::Deck);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// Prints what a key press costs on the input thread versus what the
    /// painter thread absorbs, on the connected deck. `cargo test --release
    /// -- --ignored --nocapture press_cost` (stop any running deckpad first).
    #[test]
    #[ignore]
    fn press_cost() {
        let mut cfg: Config = toml::from_str(config::DEFAULT_CONFIG).unwrap();
        cfg.drum.port_name = "deckpad-bench".into();
        cfg.drum.connect = None;
        let chord = cfg.drum.pads.iter().find(|p| p.notes.is_some()).unwrap().key;
        let hid = new_hidapi().unwrap();
        let (kind, serial) = list_devices(&hid).into_iter().next().expect("no Stream Deck connected");
        let mut app = App::new(cfg, Mode::Drum).unwrap();
        let t = Instant::now();
        app.build_faces(kind).unwrap();
        println!("encode all faces (once per connection): {:?}", t.elapsed());
        app.painter = Some(Painter::spawn(StreamDeck::connect(&hid, kind, &serial).unwrap()));
        app.enter_mode(Mode::Drum);
        app.painter.as_ref().unwrap().sync();

        let (mut total, mut worst, mut paint_worst) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
        for _ in 0..200 {
            let t = Instant::now();
            app.key_down(chord); // four MIDI notes + queue a repaint
            let down = t.elapsed();
            let t = Instant::now();
            app.painter.as_ref().unwrap().sync();
            paint_worst = paint_worst.max(t.elapsed());
            app.key_up(chord);
            app.painter.as_ref().unwrap().sync();
            total += down;
            worst = worst.max(down);
        }
        println!("chord press on input thread: avg {:?}, worst {:?}", total / 200, worst);
        println!("repaint on painter thread:   worst {paint_worst:?}");
        app.painter.take().unwrap().reset();
    }
}
