//! Drum mode output: an ALSA sequencer MIDI port.
//!
//! libasound is loaded at runtime (dlopen) rather than linked, so building
//! needs no ALSA development headers — only the library every desktop has.

use anyhow::{Context, Result, anyhow, bail};
use libloading::Library;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};

type SeqHandle = *mut c_void;

const SND_SEQ_OPEN_OUTPUT: c_int = 1;
const PORT_CAP_READ: c_uint = 1 << 0;
const PORT_CAP_SUBS_READ: c_uint = 1 << 5;
const PORT_TYPE_MIDI_GENERIC: c_uint = 1 << 1;
const PORT_TYPE_APPLICATION: c_uint = 1 << 20;
const EVENT_NOTEON: u8 = 6;
const EVENT_NOTEOFF: u8 = 7;
const EVENT_PGMCHANGE: u8 = 11;
const QUEUE_DIRECT: u8 = 253;
const ADDRESS_UNKNOWN: u8 = 253;
const ADDRESS_SUBSCRIBERS: u8 = 254;

#[repr(C)]
#[derive(Default)]
struct SeqAddr {
    client: u8,
    port: u8,
}

/// Mirrors `snd_seq_event_t` (28 bytes).
#[repr(C)]
struct SeqEvent {
    kind: u8,
    flags: u8,
    tag: u8,
    queue: u8,
    time: [u32; 2],
    source: SeqAddr,
    dest: SeqAddr,
    /// Notes use `snd_seq_ev_note_t` (channel, note, velocity, ...); program
    /// changes use `snd_seq_ev_ctrl_t` (channel, 3 unused, u32 param, i32 value).
    data: [u8; 12],
}

const _: () = assert!(std::mem::size_of::<SeqEvent>() == 28);

pub struct MidiOut {
    seq: SeqHandle,
    port: u8,
    event_output_direct: unsafe extern "C" fn(SeqHandle, *mut SeqEvent) -> c_int,
    close: unsafe extern "C" fn(SeqHandle) -> c_int,
    _lib: Library,
}

impl MidiOut {
    pub fn new(port_name: &str, connect: Option<&str>) -> Result<MidiOut> {
        // SAFETY: libasound's initialisers have no special requirements, and the
        // signatures below match alsa-lib's public seq API. Function pointers are
        // only used while `_lib` keeps the library loaded.
        unsafe {
            let lib = Library::new("libasound.so.2").context("loading libasound.so.2 (install alsa-lib)")?;
            let open: unsafe extern "C" fn(*mut SeqHandle, *const c_char, c_int, c_int) -> c_int = *lib.get(b"snd_seq_open\0")?;
            let set_client_name: unsafe extern "C" fn(SeqHandle, *const c_char) -> c_int = *lib.get(b"snd_seq_set_client_name\0")?;
            let create_simple_port: unsafe extern "C" fn(SeqHandle, *const c_char, c_uint, c_uint) -> c_int =
                *lib.get(b"snd_seq_create_simple_port\0")?;
            let parse_address: unsafe extern "C" fn(SeqHandle, *mut SeqAddr, *const c_char) -> c_int = *lib.get(b"snd_seq_parse_address\0")?;
            let connect_to: unsafe extern "C" fn(SeqHandle, c_int, c_int, c_int) -> c_int = *lib.get(b"snd_seq_connect_to\0")?;
            let event_output_direct = *lib.get(b"snd_seq_event_output_direct\0")?;
            let close = *lib.get(b"snd_seq_close\0")?;

            let mut seq: SeqHandle = std::ptr::null_mut();
            let rc = open(&mut seq, c"default".as_ptr(), SND_SEQ_OPEN_OUTPUT, 0);
            if rc < 0 || seq.is_null() {
                bail!("opening the ALSA sequencer failed (error {rc}); is /dev/snd/seq accessible?");
            }
            // From here on `out` owns the handle and closes it on any early return.
            let mut out = MidiOut { seq, port: 0, event_output_direct, close, _lib: lib };

            let name = CString::new(port_name).map_err(|_| anyhow!("drum.port_name contains a NUL byte"))?;
            set_client_name(seq, name.as_ptr());
            let port = create_simple_port(
                seq,
                name.as_ptr(),
                PORT_CAP_READ | PORT_CAP_SUBS_READ,
                PORT_TYPE_MIDI_GENERIC | PORT_TYPE_APPLICATION,
            );
            if port < 0 {
                bail!("creating the MIDI port failed (error {port})");
            }
            out.port = port as u8;

            if let Some(target) = connect {
                let target_c = CString::new(target).map_err(|_| anyhow!("drum.connect contains a NUL byte"))?;
                let mut addr = SeqAddr::default();
                if parse_address(seq, &mut addr, target_c.as_ptr()) < 0 {
                    eprintln!("deckpad: MIDI port '{target}' not found; connect manually (see `aconnect -l`)");
                } else {
                    let rc = connect_to(seq, port, addr.client as c_int, addr.port as c_int);
                    if rc < 0 {
                        eprintln!("deckpad: connecting to MIDI port '{target}' failed (error {rc})");
                    }
                }
            }
            Ok(out)
        }
    }

    /// `channel` is 0-15.
    pub fn note(&self, on: bool, channel: u8, note: u8, velocity: u8) -> Result<()> {
        let mut data = [0u8; 12];
        data[0] = channel;
        data[1] = note;
        data[2] = if on { velocity } else { 0 };
        self.send(if on { EVENT_NOTEON } else { EVENT_NOTEOFF }, data)
    }

    /// Selects an instrument (or, on channel 10, a drum kit). `channel` is 0-15.
    pub fn program(&self, channel: u8, program: u8) -> Result<()> {
        let mut data = [0u8; 12];
        data[0] = channel;
        data[8..12].copy_from_slice(&(program as i32).to_ne_bytes());
        self.send(EVENT_PGMCHANGE, data)
    }

    fn send(&self, kind: u8, data: [u8; 12]) -> Result<()> {
        let mut event = SeqEvent {
            kind,
            flags: 0,
            tag: 0,
            queue: QUEUE_DIRECT,
            time: [0; 2],
            source: SeqAddr { client: 0, port: self.port },
            dest: SeqAddr { client: ADDRESS_SUBSCRIBERS, port: ADDRESS_UNKNOWN },
            data,
        };
        // SAFETY: `seq` is a live handle and `event` is a valid snd_seq_event_t.
        let rc = unsafe { (self.event_output_direct)(self.seq, &mut event) };
        if rc < 0 {
            bail!("sending MIDI event failed (error {rc})");
        }
        Ok(())
    }
}

impl Drop for MidiOut {
    fn drop(&mut self) {
        // SAFETY: `seq` came from snd_seq_open and is closed exactly once.
        unsafe {
            (self.close)(self.seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// Sends notes through a real ALSA port and checks what `aseqdump` receives.
    /// Needs ALSA and alsa-utils: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn notes_reach_a_subscriber() {
        let midi = MidiOut::new("deckpad-test", None).unwrap();
        let mut dump = Command::new("aseqdump").args(["-p", "deckpad-test"]).stdout(Stdio::piped()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(400));
        midi.program(9, 26).unwrap();
        midi.note(true, 9, 36, 110).unwrap();
        midi.note(false, 9, 36, 110).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        dump.kill().unwrap();
        let out = String::from_utf8(dump.wait_with_output().unwrap().stdout).unwrap();
        let line = |kind: &str| out.lines().find(|l| l.contains(kind)).unwrap_or_else(|| panic!("no '{kind}' in:\n{out}")).to_string();
        assert!(line("Note on").contains("9, note 36, velocity 110"), "{out}");
        assert!(line("Program change").contains("9, program 26"), "{out}");
        assert!(line("Note off").contains("9, note 36"), "{out}");
    }
}
