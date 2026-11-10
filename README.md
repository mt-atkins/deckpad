# deckpad

A small Stream Deck daemon for Linux, written in Rust, with three modes you
cycle through from the deck itself:

| Mode | What the deck becomes |
| --- | --- |
| **deck** | Classic macro pad: key combos, media keys, shell commands |
| **gamepad** | A virtual Xbox-style controller (uinput) |
| **drum** | A MIDI performance pad with a loop recorder (ALSA sequencer port) |

![Default layouts: deck, gamepad, drum](docs/layouts.png)

It is a single ~2 MB binary with no GUI, no plugin host and no system
libraries to install: the HID backend is pure Rust and ALSA is loaded at
runtime, so it builds on immutable distros (Bazzite, Silverblue, SteamOS)
with nothing but `cargo`.

## Run

```sh
cargo run --release                 # start in the configured mode
cargo run --release -- -m drum      # start in a specific mode
cargo run --release -- --list       # show connected decks
cargo run --release -- --preview layouts.png   # render the layouts, no deck needed
```

Press the **MODE** key (bottom-left by default) to cycle deck → gamepad → drum.
Ctrl-C exits and returns the deck to its logo.

If the deck cannot be opened, install the udev rule in
[`contrib/70-deckpad.rules`](contrib/70-deckpad.rules). To start it at login,
see [`contrib/deckpad.service`](contrib/deckpad.service).

## Modes

**Gamepad.** The virtual pad uses the same button codes and USB IDs as an
Xbox 360 controller, so Steam, SDL and most games pick it up without setup.
Keys can be bound to buttons, the d-pad, triggers (full press) or stick
directions (full tilt); opposite directions cancel out. The controller is
created the first time you enter the mode and stays plugged in until exit.

**Drum.** Entering the mode creates a MIDI output port called `deckpad`.
It makes no sound by itself: connect it to a synth, sampler or DAW
(`aconnect -l` to list ports, `aconnect deckpad <target>` to wire it up, or
set `drum.connect` in the config to do that automatically).

The default layout is a house / garage performance pad: four piano chords
and two vocal stabs on top, an electronic kit underneath, and a loop
recorder down the right-hand side.

- **REC** starts a two-bar loop at 128 bpm with a click and records what you
  play. The loop keeps cycling, so build the beat up a layer per pass. Press
  again to stop recording while the loop carries on; again to overdub more.
- **PLAY** stops and restarts the loop. **CLEAR** erases it.
- Recorded hits snap to sixteenths; what you play live is never delayed.
  Pads light up as the loop plays them.

Tempo, length, grid, click, sounds (`drum.programs`) and every pad are in
the config. A pad can be one note, a chord (`notes = [...]`) or a loop
control, on any MIDI channel. Velocity is fixed per pad — the keys are not
pressure sensitive.

For sound without a DAW, [`contrib/fluidsynth.service`](contrib/fluidsynth.service)
runs FluidSynth in the background; with `connect = "FLUID Synth"` under
`[drum]` the pads play through it as soon as you enter the mode.

**Deck.** Each key sends a key combo (`keys = "ctrl+shift+t"`, any evdev key
name such as `volumeup` or `playpause`), runs a shell command, or jumps to a
mode.

## Latency

Key presses are handled in microseconds: the input thread only sends the
MIDI or controller event and queues the key repaint, and a separate thread
does the (much slower) USB display writes. Every key face is rendered and
compressed once per connection, not per press.

For drum mode the synth's audio buffer matters far more than deckpad does.
`contrib/fluidsynth.service` asks PipeWire for a 128-frame buffer (under
3 ms); PipeWire's default for it is 40-90 ms.

## Configure

```sh
mkdir -p ~/.config/deckpad
cargo run --release -- --print-config > ~/.config/deckpad/config.toml
```

The file is commented; see [`config.default.toml`](config.default.toml).
Labels, colours, key positions, gamepad bindings, drum notes, MIDI channel
and velocity are all set there. Restart to apply changes.

## Supported devices

Any Stream Deck the driver knows (Original, MK.2, Mini, XL, Neo, Plus, Pedal
and compatible modules). Only the keys are used; dials and touch strips are
ignored. Developed and tested on a Stream Deck MK.2.

## Tests

```sh
cargo test                 # parsing, config and loop timing logic
cargo test -- --ignored    # also drives a real virtual gamepad, MIDI port and Stream Deck
                           # (needs uinput, ALSA, aseqdump; stop any running deckpad first)
```

## Built on

- [`elgato-streamdeck`](https://github.com/OpenActionAPI/rust-elgato-streamdeck)
  (MPL-2.0), the device driver behind [OpenDeck](https://github.com/nekename/OpenDeck).
  It is vendored in `vendor/elgato-streamdeck` with one change: `hidapi`'s
  default C backend is switched off so the build needs no libudev headers.
- [`evdev`](https://crates.io/crates/evdev) for the virtual gamepad and keyboard.

If you want a full Stream Deck app with a GUI and Elgato plugin support, use
OpenDeck. deckpad is for when you just want the deck to *be* a controller or
a drum pad.

License: MPL-2.0.
