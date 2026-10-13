mod gui;
mod native;

use clack_extensions::audio_ports::*;
use clack_extensions::gui::PluginGui;
use clack_extensions::note_ports::*;
use clack_extensions::params::*;
use clack_extensions::state::*;
use clack_plugin::events::{spaces::CoreEventSpace, Match};
use clack_plugin::plugin::features::*;
use clack_plugin::prelude::*;
use clack_plugin::stream::{InputStream, OutputStream};
use std::cell::RefCell;
use std::fmt::Write;
use std::io::{Read, Write as _};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

// ---- parameters -------------------------------------------------------------------------------

const ATTACK: usize = 0;
const DECAY: usize = 1;
const SUSTAIN: usize = 2;
const RELEASE: usize = 3;
const VOICES: usize = 4;
const MAX_VOICES: usize = 16;

/// name, min, max, default. The index in this table is the CLAP param id.
const DEFS: [(&str, f32, f32, f32); 5] = [
    ("Attack", 0.001, 5.0, 0.01),
    ("Decay", 0.001, 5.0, 0.2),
    ("Sustain", 0.0, 1.0, 0.7),
    ("Release", 0.001, 5.0, 0.3),
    ("Voices", 1.0, MAX_VOICES as f32, 8.0),
];

/// Current param values as f32 bits. Shared between the main thread and the audio thread; the
/// audio thread is the only writer while processing, so Relaxed atomics are enough.
pub struct Params([AtomicU32; 5]);

impl Params {
    fn new() -> Self {
        Self(DEFS.map(|d| AtomicU32::new(d.3.to_bits())))
    }

    fn get(&self, i: usize) -> f32 {
        f32::from_bits(self.0[i].load(Relaxed))
    }

    fn set(&self, i: usize, v: f32) {
        if let Some(&(_, lo, hi, _)) = DEFS.get(i) {
            let v = if i == VOICES { v.round() } else { v };
            self.0[i].store(v.clamp(lo, hi).to_bits(), Relaxed);
        }
    }

    /// State format: the 5 values as little-endian f32, in param id order.
    fn to_bytes(&self) -> Vec<u8> {
        (0..DEFS.len())
            .flat_map(|i| self.get(i).to_le_bytes())
            .collect()
    }

    fn load_bytes(&self, b: &[u8]) {
        // ponytail: no version tag. Add one the day the layout changes (old saves just load by index).
        for (i, c) in b.as_chunks::<4>().0.iter().take(DEFS.len()).enumerate() {
            self.set(i, f32::from_le_bytes(*c));
        }
    }

    fn apply(&self, events: &InputEvents) {
        for e in events {
            if let Some(CoreEventSpace::ParamValue(e)) = e.as_core_event() {
                if let Some(id) = e.param_id() {
                    self.set(id.get() as usize, e.value() as f32);
                }
            }
        }
    }
}

impl PluginShared<'_> for Params {}

// ---- plugin -----------------------------------------------------------------------------------

pub struct Synth;

impl Plugin for Synth {
    type AudioProcessor<'a> = Processor<'a>;
    type Shared<'a> = Params;
    type MainThread<'a> = Main<'a>;

    fn declare_extensions(b: &mut PluginExtensions<Self>, _: Option<&Params>) {
        b.register::<PluginAudioPorts>()
            .register::<PluginNotePorts>()
            .register::<PluginParams>()
            .register::<PluginState>()
            .register::<PluginGui>();
    }
}

impl DefaultPluginFactory for Synth {
    fn get_descriptor() -> PluginDescriptor {
        PluginDescriptor::new("org.example.clap-saw-synth", "Sawwy").with_features([
            INSTRUMENT,
            SYNTHESIZER,
            STEREO,
        ])
    }

    fn new_shared(_: HostSharedHandle<'_>) -> Result<Params, PluginError> {
        Ok(Params::new())
    }

    fn new_main_thread<'a>(
        _: HostMainThreadHandle<'a>,
        params: &'a Params,
    ) -> Result<Main<'a>, PluginError> {
        Ok(Main {
            params,
            gui: RefCell::new(None),
        })
    }
}

/// Main-thread state. The GUI methods live in gui.rs.
pub struct Main<'a> {
    params: &'a Params,
    gui: RefCell<Option<gui::Open>>,
}

impl<'a> PluginMainThread<'a, Params> for Main<'a> {}

impl PluginAudioPortsImpl for Main<'_> {
    fn count(&self, is_input: bool) -> u32 {
        !is_input as u32 // one stereo output, no inputs
    }

    fn get(&self, index: u32, is_input: bool, w: &mut AudioPortInfoWriter) {
        if index == 0 && !is_input {
            w.set(&AudioPortInfo {
                id: ClapId::new(0),
                name: b"out",
                channel_count: 2,
                flags: AudioPortFlags::IS_MAIN,
                port_type: Some(AudioPortType::STEREO),
                in_place_pair: None,
            });
        }
    }
}

impl PluginNotePortsImpl for Main<'_> {
    fn count(&self, is_input: bool) -> u32 {
        is_input as u32 // one note input, no outputs
    }

    fn get(&self, index: u32, is_input: bool, w: &mut NotePortInfoWriter) {
        if index == 0 && is_input {
            w.set(&NotePortInfo {
                id: ClapId::new(0),
                name: b"notes",
                supported_dialects: NoteDialects::CLAP,
                preferred_dialect: Some(NoteDialect::Clap),
            });
        }
    }
}

impl PluginMainThreadParams for Main<'_> {
    fn count(&self) -> u32 {
        DEFS.len() as u32
    }

    fn get_info(&self, i: u32, w: &mut ParamInfoWriter) {
        let Some(&(name, min, max, default)) = DEFS.get(i as usize) else {
            return;
        };
        let stepped = if i as usize == VOICES {
            ParamInfoFlags::IS_STEPPED
        } else {
            ParamInfoFlags::empty()
        };
        w.set(&ParamInfo {
            id: ClapId::new(i),
            flags: ParamInfoFlags::IS_AUTOMATABLE | stepped,
            cookie: Default::default(),
            name: name.as_bytes(),
            module: b"",
            min_value: min as f64,
            max_value: max as f64,
            default_value: default as f64,
        });
    }

    fn get_value(&self, id: ClapId) -> Option<f64> {
        let i = id.get() as usize;
        (i < DEFS.len()).then(|| self.params.get(i) as f64)
    }

    fn value_to_text(&self, id: ClapId, v: f64, w: &mut ParamDisplayWriter) -> std::fmt::Result {
        match id.get() as usize {
            SUSTAIN => write!(w, "{:.0} %", v * 100.0),
            VOICES => write!(w, "{:.0}", v),
            _ => write!(w, "{:.3} s", v),
        }
    }

    fn text_to_value(&self, _: ClapId, text: &std::ffi::CStr) -> Option<f64> {
        // "0.5", "0.5 s" or "70 %"; sustain's % is mapped back by the host's own clamping
        let t = text
            .to_str()
            .ok()?
            .trim()
            .trim_end_matches(['s', '%'])
            .trim();
        t.parse().ok()
    }

    // Host flushes params while the plugin is inactive (no audio processor to receive them).
    fn flush(&self, input: &InputEvents, _: &mut OutputEvents) {
        self.params.apply(input);
    }
}

impl PluginStateImpl for Main<'_> {
    fn save(&self, out: &mut OutputStream) -> Result<(), PluginError> {
        Ok(out.write_all(&self.params.to_bytes())?)
    }

    fn load(&self, input: &mut InputStream) -> Result<(), PluginError> {
        let mut b = Vec::new();
        input.read_to_end(&mut b)?;
        self.params.load_bytes(&b);
        Ok(())
    }
}

// ---- synth engine -----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stage {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    stage: Stage,
    key: u16,
    freq: f32,
    phase: f32,
    level: f32,
    rel_step: f32, // per-sample decrement, fixed at note-off
    age: u64,      // note-on counter, for stealing the oldest voice
}

const IDLE: Voice = Voice {
    stage: Stage::Off,
    key: 0,
    freq: 0.0,
    phase: 0.0,
    level: 0.0,
    rel_step: 0.0,
    age: 0,
};

/// Polyphonic saw synth with a linear ADSR per voice. Plain Copy data: process() renders each
/// output channel from a copy of the same starting state, which keeps channels identical.
// ponytail: naive (aliased) saw, linear envelope, block-accurate note events, oldest-voice
// stealing. polyBLEP / exponential curves / sample-accurate events if it matters.
#[derive(Clone, Copy)]
pub struct Engine {
    voices: [Voice; MAX_VOICES],
    adsr: [f32; 4], // attack s, decay s, sustain level, release s
    n: usize,       // active polyphony limit
    sr: f32,
    clock: u64,
}

impl Engine {
    pub fn new(sr: f32) -> Self {
        Self {
            voices: [IDLE; MAX_VOICES],
            adsr: [0.01, 0.2, 0.7, 0.3],
            n: 8,
            sr,
            clock: 0,
        }
    }

    fn load(&mut self, p: &Params) {
        self.adsr = [p.get(ATTACK), p.get(DECAY), p.get(SUSTAIN), p.get(RELEASE)];
        self.n = p.get(VOICES) as usize;
        for v in &mut self.voices[self.n..] {
            if v.stage != Stage::Off {
                v.stage = Stage::Release; // polyphony lowered: let the extra voices fade out
                v.rel_step = v.level / (self.adsr[3] * self.sr);
            }
        }
    }

    pub fn note_on(&mut self, key: u16) {
        self.clock += 1;
        let v = &mut self.voices[..self.n];
        let i = v
            .iter()
            .position(|v| v.stage == Stage::Off)
            .or_else(|| (0..v.len()).min_by_key(|&i| v[i].age)) // all busy: steal the oldest
            .unwrap();
        v[i] = Voice {
            stage: Stage::Attack,
            key,
            freq: 440.0 * 2f32.powf((key as f32 - 69.0) / 12.0),
            level: v[i].level, // keep the old level when stealing: no click
            age: self.clock,
            ..IDLE
        };
    }

    pub fn note_off(&mut self, key: u16) {
        for v in &mut self.voices {
            if v.key == key && !matches!(v.stage, Stage::Off | Stage::Release) {
                v.stage = Stage::Release;
                v.rel_step = v.level / (self.adsr[3] * self.sr);
            }
        }
    }

    pub fn active(&self) -> usize {
        self.voices.iter().filter(|v| v.stage != Stage::Off).count()
    }

    pub fn next_sample(&mut self) -> f32 {
        let [a, d, s, _] = self.adsr;
        let mut out = 0.0;
        for v in &mut self.voices {
            match v.stage {
                Stage::Off => continue,
                Stage::Attack => {
                    v.level += 1.0 / (a * self.sr);
                    if v.level >= 1.0 {
                        (v.level, v.stage) = (1.0, Stage::Decay);
                    }
                }
                Stage::Decay => {
                    v.level -= (1.0 - s) / (d * self.sr);
                    if v.level <= s {
                        (v.level, v.stage) = (s, Stage::Sustain);
                    }
                }
                Stage::Sustain => v.level = s,
                Stage::Release => {
                    v.level -= v.rel_step;
                    if v.level <= 0.0 {
                        *v = IDLE;
                        continue;
                    }
                }
            }
            v.phase = (v.phase + v.freq / self.sr).fract();
            out += (v.phase * 2.0 - 1.0) * v.level;
        }
        out * 0.3 / (self.n as f32).sqrt() // headroom that shrinks with polyphony
    }
}

pub struct Processor<'a> {
    params: &'a Params,
    engine: Engine,
}

impl<'a> PluginAudioProcessor<'a, Params, Main<'a>> for Processor<'a> {
    fn activate(
        _: HostAudioProcessorHandle<'a>,
        _: &Main<'a>,
        params: &'a Params,
        cfg: PluginAudioConfiguration,
    ) -> Result<Self, PluginError> {
        Ok(Self {
            params,
            engine: Engine::new(cfg.sample_rate as f32),
        })
    }

    fn process(
        &mut self,
        _: Process,
        mut audio: Audio,
        events: Events,
    ) -> Result<ProcessStatus, PluginError> {
        self.params.apply(events.input);
        self.engine.load(self.params);
        for e in events.input {
            match e.as_core_event() {
                Some(CoreEventSpace::NoteOn(e)) => {
                    if let Match::Specific(k) = e.key() {
                        self.engine.note_on(k)
                    }
                }
                Some(CoreEventSpace::NoteOff(e)) => {
                    if let Match::Specific(k) = e.key() {
                        self.engine.note_off(k)
                    }
                }
                _ => {}
            }
        }

        let start = self.engine;
        let mut end = start;
        for mut port in &mut audio {
            let Some(chans) = port.channels()?.into_f32() else {
                continue;
            };
            for ch in chans {
                if let ChannelPair::OutputOnly(buf) = ch {
                    let mut e = start; // every channel renders the same mono signal
                    for s in buf {
                        *s = e.next_sample();
                    }
                    end = e;
                }
            }
        }
        self.engine = end;
        Ok(ProcessStatus::ContinueIfNotQuiet)
    }
}

impl PluginAudioProcessorParams for Processor<'_> {
    fn flush(&mut self, input: &InputEvents, _: &mut OutputEvents) {
        self.params.apply(input);
    }
}

clack_export_entry!(SinglePluginEntry<Synth>);

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let mut e = Engine::new(48000.0);
        e.load(&Params::new());
        e
    }

    #[test]
    fn adsr_attacks_sustains_then_releases_to_silence() {
        let mut e = engine();
        assert_eq!(e.next_sample(), 0.0);
        e.note_on(69);
        let peak = (0..48000)
            .map(|_| e.next_sample().abs())
            .fold(0.0, f32::max);
        assert!(peak > 0.05 && peak < 0.3, "peak {peak}");
        assert_eq!(e.voices[0].stage, Stage::Sustain);
        e.note_off(69);
        (0..48000).for_each(|_| _ = e.next_sample()); // release is 0.3 s
        assert_eq!(e.active(), 0);
        assert_eq!(e.next_sample(), 0.0);
    }

    #[test]
    fn polyphony_is_limited_by_the_voices_param() {
        let p = Params::new();
        p.set(VOICES, 2.0);
        let mut e = Engine::new(48000.0);
        e.load(&p);
        [60, 64, 67].iter().for_each(|&k| e.note_on(k));
        assert_eq!(e.active(), 2);
        // the oldest (60) was stolen, 64 and 67 remain
        assert!(e.voices[..2].iter().all(|v| v.key != 60));
        // lowering the limit fades the extra voices out
        p.set(VOICES, 1.0);
        e.load(&p);
        (0..48000).for_each(|_| _ = e.next_sample());
        assert_eq!(e.active(), 1);
    }

    #[test]
    fn state_roundtrips() {
        let (a, b) = (Params::new(), Params::new());
        a.set(ATTACK, 1.5);
        a.set(SUSTAIN, 0.25);
        a.set(VOICES, 5.0);
        b.load_bytes(&a.to_bytes());
        assert!((0..DEFS.len()).all(|i| a.get(i) == b.get(i)));
        b.load_bytes(&[1, 2, 3]); // truncated/garbage state must not panic or change anything
        assert_eq!(b.get(VOICES), 5.0);
    }

    #[test]
    fn params_clamp_and_voices_round() {
        let p = Params::new();
        p.set(VOICES, 99.0);
        assert_eq!(p.get(VOICES), 16.0);
        p.set(VOICES, 3.4);
        assert_eq!(p.get(VOICES), 3.0);
        p.set(SUSTAIN, -1.0);
        assert_eq!(p.get(SUSTAIN), 0.0);
        p.set(42, 1.0); // unknown id is ignored
    }
}
