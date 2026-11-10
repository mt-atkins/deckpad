//! Background thread that owns all writes to the deck's key displays.
//!
//! Sending an image to a key takes a few milliseconds of USB transfers (and
//! occasionally tens). Doing that on the input thread would delay the next
//! key press, so the input thread only queues images here and moves on.

use elgato_streamdeck::StreamDeck;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

enum Cmd {
    /// Show an encoded image on a key (`None` blanks it).
    Key(u8, Option<Arc<Vec<u8>>>),
    /// Return the deck to its logo.
    Reset,
    /// Reply once everything queued before this has been written.
    #[cfg_attr(not(test), allow(dead_code))]
    Sync(SyncSender<()>),
}

pub struct Painter {
    tx: Option<Sender<Cmd>>,
    thread: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
}

impl Painter {
    /// Takes its own handle to the device; reads happen on a separate handle.
    pub fn spawn(deck: StreamDeck) -> Painter {
        let (tx, rx) = channel();
        let failed = Arc::new(AtomicBool::new(false));
        let flag = failed.clone();
        let thread = std::thread::Builder::new()
            .name("deckpad-painter".into())
            .spawn(move || {
                if run(&deck, rx).is_err() {
                    flag.store(true, Ordering::Relaxed);
                }
            })
            .expect("spawning painter thread");
        Painter { tx: Some(tx), thread: Some(thread), failed }
    }

    fn send(&self, cmd: Cmd) {
        // A send only fails once the thread has died; `failed()` reports that.
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    pub fn image(&self, key: u8, data: Arc<Vec<u8>>) {
        self.send(Cmd::Key(key, Some(data)));
    }

    pub fn clear(&self, key: u8) {
        self.send(Cmd::Key(key, None));
    }

    pub fn reset(&self) {
        self.send(Cmd::Reset);
    }

    /// Blocks until everything queued so far is on the device (used by tests).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sync(&self) {
        let (done_tx, done_rx) = sync_channel(1);
        self.send(Cmd::Sync(done_tx));
        let _ = done_rx.recv();
    }

    /// True once a write has failed (device unplugged); the painter has stopped.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        self.tx = None; // closes the queue; the thread drains it and exits
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(deck: &StreamDeck, rx: Receiver<Cmd>) -> Result<(), elgato_streamdeck::StreamDeckError> {
    while let Ok(first) = rx.recv() {
        // Take everything already queued and keep only the newest image per
        // key: if we fell behind, stale frames are skipped rather than drawn late.
        let mut newest: HashMap<u8, Option<Arc<Vec<u8>>>> = HashMap::new();
        let mut waiters = Vec::new();
        let mut reset = false;
        for cmd in std::iter::once(first).chain(rx.try_iter()) {
            match cmd {
                Cmd::Key(key, image) => {
                    newest.insert(key, image);
                }
                Cmd::Reset => {
                    newest.clear();
                    reset = true;
                }
                Cmd::Sync(done) => waiters.push(done),
            }
        }
        if reset {
            deck.reset()?;
        }
        if !newest.is_empty() {
            for (key, image) in &newest {
                match image {
                    Some(data) => deck.write_image(*key, data)?,
                    None => deck.clear_button_image(*key)?,
                }
            }
            deck.flush()?;
        }
        for done in waiters {
            let _ = done.send(());
        }
    }
    Ok(())
}
