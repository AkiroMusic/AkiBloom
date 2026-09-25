//! AkiBloom — grow playable timbres from your own samples.
//!
//! Data flow: seeds (the producer's own samples) → offline render on a
//! worker thread (graft toward the other seed's timbre profile, optionally a
//! variation transform) → a looping [`PlayableVariant`] → eight polyphonic
//! voices driven by MIDI or the GUI's virtual keyboard. The audio thread
//! never renders and never touches the file system; it only swaps in new
//! `Arc`s the workers publish.

use nih_plug::params::enums::Enum;
use nih_plug::prelude::*;
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub mod graft;
mod gui;
pub mod mel;
pub mod parents;
pub mod render;
pub mod seeds;
pub mod stft;
pub mod voices;
pub mod wav;

use render::PlayableVariant;
use seeds::SharedSeeds;
use voices::Voices;

/// Which seed slot is the *source* being played (the other one, when loaded,
/// donates its timbre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum SeedSide {
    #[name = "A"]
    A,
    #[name = "B"]
    B,
}

impl SeedSide {
    fn slot(self) -> seeds::Slot {
        match self {
            SeedSide::A => seeds::Slot::A,
            SeedSide::B => seeds::Slot::B,
        }
    }

    fn other(self) -> seeds::Slot {
        match self {
            SeedSide::A => seeds::Slot::B,
            SeedSide::B => seeds::Slot::A,
        }
    }
}

/// The GUI's virtual keyboard: the currently held note, if any. Written by
/// the editor, polled by the audio thread.
pub type VirtualNote = Arc<Mutex<Option<u8>>>;

/// The looping rendered timbre, swapped in atomically by render workers.
pub type SharedVariant = Arc<parking_lot::RwLock<Option<Arc<PlayableVariant>>>>;

pub struct AkiBloom {
    params: Arc<AkiBloomParams>,
    seeds: SharedSeeds,
    variant: SharedVariant,
    /// Status line shared with load/render workers (GUI + workers only).
    status: Arc<Mutex<String>>,
    /// The GUI keyboard's held note.
    virtual_note: VirtualNote,
    /// Last virtual note the audio thread saw (for on/off transitions).
    last_virtual: Option<u8>,
    voices: Voices,
    /// Context sample rate, set in `initialize` (48 kHz until then).
    sample_rate: f32,
}

#[derive(Params)]
pub struct AkiBloomParams {
    /// Which seed is the played source (the other donates its color).
    #[id = "side"]
    pub side: EnumParam<SeedSide>,
    /// Grow amount: how far the source is pulled toward the donor's timbre
    /// (0 = the seed as-is, 1 = fully the donor's color). Applied at render
    /// time, so changes re-render on the worker.
    #[id = "alpha"]
    pub alpha: FloatParam,
    /// MIDI note that plays the variant at its native pitch.
    #[id = "root"]
    pub root: IntParam,
    /// Output level.
    #[id = "gain"]
    pub gain: FloatParam,

    /// Persisted seed paths so a saved session reloads its sources.
    #[persist = "seed-a-path"]
    pub seed_a_path: Arc<parking_lot::Mutex<String>>,
    #[persist = "seed-b-path"]
    pub seed_b_path: Arc<parking_lot::Mutex<String>>,
}

impl Default for AkiBloom {
    fn default() -> Self {
        Self {
            params: Arc::new(AkiBloomParams::default()),
            seeds: Arc::new(parking_lot::RwLock::new(seeds::SeedPair::default())),
            variant: Arc::new(parking_lot::RwLock::new(None)),
            status: Arc::new(Mutex::new(String::new())),
            virtual_note: Arc::new(Mutex::new(None)),
            last_virtual: None,
            voices: Voices::new(),
            sample_rate: 48_000.0,
        }
    }
}

impl Default for AkiBloomParams {
    fn default() -> Self {
        Self::new()
    }
}

impl AkiBloomParams {
    pub fn new() -> Self {
        Self {
            side: EnumParam::new("源 Source", SeedSide::A),
            // Instant gratification after loading two seeds — the graft is
            // audible immediately; α = 0 remains the identity (原声).
            alpha: FloatParam::new(
                "生长量 α",
                0.7,
                FloatRange::Linear {
                    min: 0.0,
                    max: 1.0,
                },
            )
            .with_unit(" %")
            .with_value_to_string(formatters::v2s_f32_percentage(0))
            .with_string_to_value(formatters::s2v_f32_percentage()),
            root: IntParam::new(
                "根音 Root",
                60,
                IntRange::Linear {
                    min: 24,
                    max: 96,
                },
            ),
            gain: FloatParam::new(
                "输出 Output",
                util::db_to_gain(0.0),
                FloatRange::Skewed {
                    min: util::db_to_gain(-24.0),
                    max: util::db_to_gain(24.0),
                    factor: FloatRange::gain_skew_factor(-24.0, 24.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(50.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),
            seed_a_path: Arc::new(parking_lot::Mutex::new(String::new())),
            seed_b_path: Arc::new(parking_lot::Mutex::new(String::new())),
        }
    }
}

impl AkiBloom {
    /// The rendered variant to play this block (cheap `Arc` clone).
    fn current_variant(&self) -> Option<Arc<PlayableVariant>> {
        self.variant.read().clone()
    }

    /// Reload persisted seed files on worker threads (a saved session
    /// restores its sources). Skipped when the slot is already populated.
    fn reload_persisted_seeds(&mut self) {
        for (path_lock, slot) in [
            (self.params.seed_a_path.clone(), seeds::Slot::A),
            (self.params.seed_b_path.clone(), seeds::Slot::B),
        ] {
            let path = path_lock.lock().clone();
            if path.is_empty() {
                continue;
            }
            let seeds = self.seeds.clone();
            let status = self.status.clone();
            std::thread::spawn(move || {
                if seeds.read().get(slot).is_some() {
                    return;
                }
                match seeds::load_seed(Path::new(&path)) {
                    Ok(seed) => {
                        let info = seed.info_line();
                        seeds.write().set(slot, Arc::new(seed));
                        *status.lock().unwrap() = format!("已恢复种子 {info}");
                    }
                    Err(e) => {
                        *status.lock().unwrap() = format!("种子恢复失败（{path}）：{e}");
                    }
                }
            });
        }
    }
}

impl Plugin for AkiBloom {
    const NAME: &'static str = "AkiBloom";
    const VENDOR: &'static str = "AkiroMusic";
    const URL: &'static str = "https://github.com/AkiroMusic/AkiBloom";
    const EMAIL: &'static str = "akiromusic@qq.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// Instrument: stereo output, no audio input.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        aux_input_ports: &[],
        aux_output_ports: &[],
        names: PortNames::const_default(),
    }];
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = true;
    const HARD_REALTIME_ONLY: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn initialize(
        &mut self,
        _audio_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate as f32;
        self.reload_persisted_seeds();
        true
    }

    fn reset(&mut self) {
        self.voices.reset();
        self.last_virtual = None;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let channels = buffer.as_slice();
        for chan in channels.iter_mut() {
            chan.fill(0.0);
        }

        // Host MIDI events.
        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOn { note, velocity, .. } => {
                    let root = self.params.root.value() as u8;
                    self.voices.note_on(note, velocity, root);
                }
                NoteEvent::NoteOff { note, .. } => {
                    self.voices.note_off(note);
                }
                _ => {}
            }
        }

        // The GUI keyboard's held note, as on/off transitions.
        let virt = *self.virtual_note.lock().unwrap();
        if virt != self.last_virtual {
            if let Some(note) = self.last_virtual {
                self.voices.note_off(note);
            }
            if let Some(note) = virt {
                let root = self.params.root.value() as u8;
                self.voices.note_on(note, 0.9, root);
            }
            self.last_virtual = virt;
        }

        if self.voices.idle() {
            return ProcessStatus::Normal;
        }

        let Some(variant) = self.current_variant() else {
            return ProcessStatus::Normal;
        };
        let ratio = self.sample_rate as f64 / variant.sample_rate.max(1) as f64;
        self.voices.render_block(
            channels,
            Some(&variant),
            ratio,
            self.params.gain.value(),
            self.sample_rate,
        );

        ProcessStatus::Normal
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        gui::create(
            self.params.clone(),
            self.seeds.clone(),
            self.variant.clone(),
            self.status.clone(),
            self.virtual_note.clone(),
        )
    }
}

impl ClapPlugin for AkiBloom {
    const CLAP_ID: &'static str = "com.akiro.music.akibloom";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Grow playable timbres from your own samples — organic operations");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] =
        &[ClapFeature::Instrument, ClapFeature::Stereo];
}

impl Vst3Plugin for AkiBloom {
    const VST3_CLASS_ID: [u8; 16] = *b"AkiBloomAkiroM01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Sampler];
}

nih_export_clap!(AkiBloom);
nih_export_vst3!(AkiBloom);
