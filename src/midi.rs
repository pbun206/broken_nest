use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{Arc, Mutex};

/// A MIDI message that can be sent to a plugin via [`MidiSender`].
///
/// Covers the most common channel messages. Use [`MidiEvent::Raw`] for
/// anything else (SysEx is not supported — max 3 bytes).
#[derive(Debug, Clone, Copy)]
pub enum MidiEvent {
    NoteOn { channel: u8, note: u8, velocity: u8 },
    NoteOff { channel: u8, note: u8, velocity: u8 },
    Cc { channel: u8, controller: u8, value: u8 },
    PitchBend { channel: u8, value: i16 },
    ProgramChange { channel: u8, program: u8 },
    Raw { len: u8, data: [u8; 3] },
}

impl MidiEvent {
    /// Encode as raw MIDI bytes. Returns `(length, bytes)` — only the first
    /// `length` bytes of the array are valid.
    pub fn to_bytes(&self) -> (usize, [u8; 3]) {
        match *self {
            MidiEvent::NoteOn { channel, note, velocity } => {
                (3, [0x90 | (channel & 0x0F), note & 0x7F, velocity & 0x7F])
            }
            MidiEvent::NoteOff { channel, note, velocity } => {
                (3, [0x80 | (channel & 0x0F), note & 0x7F, velocity & 0x7F])
            }
            MidiEvent::Cc { channel, controller, value } => {
                (3, [0xB0 | (channel & 0x0F), controller & 0x7F, value & 0x7F])
            }
            MidiEvent::PitchBend { channel, value } => {
                let centered = (value + 8192) as u16;
                let lsb = (centered & 0x7F) as u8;
                let msb = ((centered >> 7) & 0x7F) as u8;
                (3, [0xE0 | (channel & 0x0F), lsb, msb])
            }
            MidiEvent::ProgramChange { channel, program } => {
                (2, [0xC0 | (channel & 0x0F), program & 0x7F, 0])
            }
            MidiEvent::Raw { len, data } => (len as usize, data),
        }
    }
}

/// Cloneable sender handle for pushing MIDI events into a plugin's ring buffer.
/// Obtained via [`Chain::midi_sender`](crate::Chain::midi_sender).
#[derive(Clone)]
pub struct MidiSender {
    producer: Arc<Mutex<Producer<MidiEvent>>>,
}

impl MidiSender {
    /// Enqueue a MIDI event. Returns `Err(event)` if the ring buffer is full.
    pub fn send(&self, event: MidiEvent) -> Result<(), MidiEvent> {
        // `rtrb` is SPSC, so multiple sender handles serialize access to the
        // single producer via a mutex while still sharing the same queue.
        let mut producer = self.producer.lock().map_err(|_| event)?;
        producer.push(event).map_err(|rtrb::PushError::Full(v)| v)
    }
}

/// Consumer side of a single plugin's MIDI channel, paired with the JACK
/// output port name it will be registered under.
pub(crate) struct MidiPort {
    pub name: String,
    pub consumer: Consumer<MidiEvent>,
}

/// Create a lock-free MIDI channel: a [`MidiSender`] for the caller and a
/// [`MidiPort`] that the [`MidiRouter`] will drain in the JACK process callback.
pub(crate) fn create_midi_channel(name: &str, capacity: usize) -> (MidiSender, MidiPort) {
    let (producer, consumer) = RingBuffer::new(capacity);
    let sender = MidiSender {
        producer: Arc::new(Mutex::new(producer)),
    };
    let port = MidiPort {
        name: name.to_string(),
        consumer,
    };
    (sender, port)
}

/// JACK client that owns one MIDI output port per plugin and drains the
/// corresponding ring buffers in the real-time process callback.
pub(crate) struct MidiRouter {
    _client: jack::AsyncClient<(), MidiProcessHandler>,
}

/// Real-time JACK process handler. Each cycle, drains every ring buffer and
/// writes the encoded MIDI bytes to the corresponding JACK MIDI output port.
pub(crate) struct MidiProcessHandler {
    ports: Vec<(jack::Port<jack::MidiOut>, Consumer<MidiEvent>)>,
}

impl jack::ProcessHandler for MidiProcessHandler {
    fn process(&mut self, _client: &jack::Client, ps: &jack::ProcessScope) -> jack::Control {
        for (port, consumer) in &mut self.ports {
            let mut writer = port.writer(ps);
            while let Ok(event) = consumer.pop() {
                let (len, bytes) = event.to_bytes();
                let _ = writer.write(&jack::RawMidi {
                    time: 0,
                    bytes: &bytes[..len],
                });
            }
        }
        jack::Control::Continue
    }
}

impl MidiRouter {
    /// Create and activate a JACK client with one MIDI output port per
    /// [`MidiPort`]. The client immediately starts processing audio cycles.
    pub fn new(
        client_name: &str,
        ports: Vec<MidiPort>,
    ) -> Result<Self, crate::error::Error> {
        let (client, _status) = jack::Client::new(
            client_name,
            jack::ClientOptions::NO_START_SERVER,
        )
        .map_err(|e| crate::error::Error::Wiring(format!("JACK client failed: {e}")))?;

        let mut jack_ports = Vec::with_capacity(ports.len());
        for midi_port in ports {
            let port = client
                .register_port(&midi_port.name, jack::MidiOut::default())
                .map_err(|e| {
                    crate::error::Error::Wiring(format!(
                        "failed to register MIDI port '{}': {e}",
                        midi_port.name
                    ))
                })?;
            jack_ports.push((port, midi_port.consumer));
        }

        let handler = MidiProcessHandler { ports: jack_ports };
        let active = client.activate_async((), handler).map_err(|e| {
            crate::error::Error::Wiring(format!("JACK activate failed: {e}"))
        })?;

        Ok(Self {
            _client: active,
        })
    }
}
