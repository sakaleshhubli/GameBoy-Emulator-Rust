// Game Boy DMG Audio Processing Unit.
//
// Ticks alongside the CPU (per T-cycle, same pattern as the timer/PPU),
// generates each channel's waveform internally, mixes them down through
// NR50/NR51, and buffers real f32 samples at whatever rate the host audio
// device wants. main.rs drains that buffer into an actual cpal output
// stream - this module doesn't know or care that cpal exists.
//
// Channel 1: square wave + frequency sweep + volume envelope
// Channel 2: square wave + volume envelope
// Channel 3: wave channel (32 4-bit samples from wave RAM)
// Channel 4: noise channel (LFSR)
//
// Known simplifications (consistent with similar notes elsewhere in this
// codebase): the frequency sweep only performs the overflow check once
// per step, not the double-check real hardware does; the envelope timer
// doesn't reproduce the "zombie mode" quirk from writing NRx2 mid-note.
// Neither affects normal gameplay audio.

const DUTY_TABLE: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [1, 0, 0, 0, 0, 0, 0, 1], // 25%
    [1, 0, 0, 0, 0, 1, 1, 1], // 50%
    [0, 1, 1, 1, 1, 1, 1, 0], // 75%
];

const NOISE_DIVISORS: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

// Volume envelope, shared by channels 1, 2 and 4.
#[derive(Default)]
struct Envelope {
    initial_volume: u8,
    direction_increase: bool,
    period: u8,

    current_volume: u8,
    timer: u8,
}

impl Envelope {
    fn trigger(&mut self) {
        self.current_volume = self.initial_volume;
        self.timer = self.period;
    }

    // Clocked at 64 Hz by the frame sequencer (step 7)
    fn tick(&mut self) {
        // A period of 0 disables the envelope - volume just stays put.
        if self.period == 0 {
            return;
        }

        if self.timer > 0 {
            self.timer -= 1;
        }

        if self.timer == 0 {
            self.timer = self.period;

            if self.direction_increase && self.current_volume < 15 {
                self.current_volume += 1;
            } else if !self.direction_increase && self.current_volume > 0 {
                self.current_volume -= 1;
            }
        }
    }
}

enum SweepResult {
    NoChange,
    UpdateFrequency(u16),
    Disable,
}

// Frequency sweep - channel 1 only.
#[derive(Default)]
struct Sweep {
    period: u8,
    negate: bool,
    shift: u8,

    timer: u8,
    enabled: bool,
    shadow_frequency: u16,
}

impl Sweep {
    // Returns false if the channel should be disabled immediately as a
    // result of triggering (an overflow found during the initial check).
    fn trigger(&mut self, frequency: u16) -> bool {
        self.shadow_frequency = frequency;
        self.timer = if self.period == 0 { 8 } else { self.period };
        self.enabled = self.period != 0 || self.shift != 0;

        if self.shift != 0 {
            self.calculate() <= 2047
        } else {
            true
        }
    }

    fn calculate(&self) -> u16 {
        let delta = self.shadow_frequency >> self.shift;
        if self.negate {
            self.shadow_frequency.wrapping_sub(delta)
        } else {
            self.shadow_frequency.wrapping_add(delta)
        }
    }

    // Clocked at 128 Hz by the frame sequencer (steps 2 and 6)
    fn tick(&mut self) -> SweepResult {
        if !self.enabled || self.period == 0 {
            return SweepResult::NoChange;
        }

        if self.timer > 0 {
            self.timer -= 1;
        }

        if self.timer != 0 {
            return SweepResult::NoChange;
        }

        self.timer = self.period;

        let new_freq = self.calculate();
        if new_freq > 2047 {
            return SweepResult::Disable;
        }

        self.shadow_frequency = new_freq;

        if self.shift != 0 {
            SweepResult::UpdateFrequency(new_freq)
        } else {
            SweepResult::NoChange
        }
    }
}

// Channels 1 and 2 - both are duty-cycle square waves, channel 2 just has
// no sweep attached to it in Sound.
#[derive(Default)]
struct SquareChannel {
    enabled: bool,
    dac_enabled: bool,

    duty: u8,
    duty_position: u8,

    length_counter: u16,
    length_enabled: bool,

    frequency: u16,
    frequency_timer: u32,

    envelope: Envelope,
}

impl SquareChannel {
    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;

        if self.length_counter == 0 {
            self.length_counter = 64;
        }

        self.frequency_timer = (2048 - self.frequency as u32) * 4;
        self.envelope.trigger();
    }

    fn tick(&mut self) {
        if !self.enabled {
            return;
        }

        if self.frequency_timer > 0 {
            self.frequency_timer -= 1;
        }

        if self.frequency_timer == 0 {
            self.frequency_timer = (2048 - self.frequency as u32) * 4;
            self.duty_position = (self.duty_position + 1) & 7;
        }
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 {
                self.enabled = false;
            }
        }
    }

    fn amplitude(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        DUTY_TABLE[self.duty as usize][self.duty_position as usize] * self.envelope.current_volume
    }
}

// Channel 3 - plays back 32 4-bit samples from wave RAM (owned by Sound,
// since it's also directly CPU-addressable at 0xFF30-0xFF3F).
#[derive(Default)]
struct WaveChannel {
    enabled: bool,
    dac_enabled: bool,

    volume_code: u8, // 0=mute, 1=100%, 2=50%, 3=25%

    length_counter: u16,
    length_enabled: bool,

    frequency: u16,
    frequency_timer: u32,
    wave_position: u8, // 0-31
}

impl WaveChannel {
    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;

        if self.length_counter == 0 {
            self.length_counter = 256;
        }

        self.frequency_timer = (2048 - self.frequency as u32) * 2;
        self.wave_position = 0;
    }

    fn tick(&mut self) {
        if !self.enabled {
            return;
        }

        if self.frequency_timer > 0 {
            self.frequency_timer -= 1;
        }

        if self.frequency_timer == 0 {
            self.frequency_timer = (2048 - self.frequency as u32) * 2;
            self.wave_position = (self.wave_position + 1) & 31;
        }
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 {
                self.enabled = false;
            }
        }
    }

    fn amplitude(&self, wave_ram: &[u8; 16]) -> u8 {
        if !self.enabled {
            return 0;
        }

        let byte = wave_ram[(self.wave_position / 2) as usize];
        let nibble = if self.wave_position % 2 == 0 {
            byte >> 4
        } else {
            byte & 0x0F
        };

        match self.volume_code {
            0 => 0,
            1 => nibble,
            2 => nibble >> 1,
            3 => nibble >> 2,
            _ => 0,
        }
    }
}

// Channel 4 - pseudo-random noise via a linear feedback shift register.
#[derive(Default)]
struct NoiseChannel {
    enabled: bool,
    dac_enabled: bool,

    length_counter: u16,
    length_enabled: bool,

    envelope: Envelope,

    clock_shift: u8,
    width_mode_7bit: bool,
    divisor_code: u8,

    lfsr: u16,
    frequency_timer: u32,
}

impl NoiseChannel {
    fn trigger(&mut self) {
        self.enabled = self.dac_enabled;

        if self.length_counter == 0 {
            self.length_counter = 64;
        }

        self.envelope.trigger();
        self.lfsr = 0x7FFF;
        self.frequency_timer = NOISE_DIVISORS[self.divisor_code as usize] << self.clock_shift;
    }

    fn tick(&mut self) {
        if !self.enabled {
            return;
        }

        if self.frequency_timer > 0 {
            self.frequency_timer -= 1;
        }

        if self.frequency_timer == 0 {
            self.frequency_timer = NOISE_DIVISORS[self.divisor_code as usize] << self.clock_shift;

            let xor_bit = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
            self.lfsr >>= 1;
            self.lfsr |= xor_bit << 14;

            if self.width_mode_7bit {
                self.lfsr &= !(1 << 6);
                self.lfsr |= xor_bit << 6;
            }
        }
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_counter > 0 {
            self.length_counter -= 1;
            if self.length_counter == 0 {
                self.enabled = false;
            }
        }
    }

    fn amplitude(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        // Output bit is the LFSR's low bit, inverted.
        let bit = (!self.lfsr) & 1;
        (bit as u8) * self.envelope.current_volume
    }
}

pub struct Sound {
    // Sound control registers
    nr50: u8,
    nr51: u8,

    // Whether the APU as a whole is on (NR52 bit 7). When off, all
    // channels are silenced and the frame sequencer doesn't run - real
    // hardware also can't be written to (except NR52 itself) while off,
    // which we don't enforce, but nothing will actually sound.
    enabled: bool,

    channel1: SquareChannel,
    sweep: Sweep,
    channel2: SquareChannel,
    channel3: WaveChannel,
    channel4: NoiseChannel,

    // Wave RAM: FF30-FF3F, 32 4-bit samples packed two per byte
    wave_ram: [u8; 16],

    // 512 Hz frame sequencer that drives length/envelope/sweep timing
    frame_sequencer_step: u8,
    frame_sequencer_counter: u32,

    // Sample generation: accumulate T-cycles until we've built up enough
    // for one output sample at the host's sample rate, then mix and push.
    // Using a float accumulator (rather than an integer divide) avoids
    // pitch drift from the non-integer T-cycles-per-sample ratio.
    sample_rate: f64,
    cycles_per_sample: f64,
    sample_accumulator: f64,
    sample_buffer: Vec<f32>,
}

impl Sound {
    pub fn new(sample_rate: u32) -> Self {
        let sample_rate = sample_rate as f64;

        Self {
            nr50: 0,
            nr51: 0,
            enabled: false,

            channel1: SquareChannel::default(),
            sweep: Sweep::default(),
            channel2: SquareChannel::default(),
            channel3: WaveChannel::default(),
            channel4: NoiseChannel::default(),

            wave_ram: [0; 16],

            frame_sequencer_step: 0,
            frame_sequencer_counter: 0,

            sample_rate,
            cycles_per_sample: 4_194_304.0 / sample_rate,
            sample_accumulator: 0.0,
            sample_buffer: Vec::new(),
        }
    }

    pub fn read(&self, address: u16) -> u8 {
        match address {
            // Channel 1
            0xFF10 => {
                0x80 | (self.sweep.period << 4)
                    | ((self.sweep.negate as u8) << 3)
                    | self.sweep.shift
            }
            0xFF11 => (self.channel1.duty << 6) | 0x3F,
            0xFF12 => {
                (self.channel1.envelope.initial_volume << 4)
                    | ((self.channel1.envelope.direction_increase as u8) << 3)
                    | self.channel1.envelope.period
            }
            0xFF13 => 0xFF, // write-only
            0xFF14 => 0xBF | ((self.channel1.length_enabled as u8) << 6),

            // Channel 2
            0xFF16 => (self.channel2.duty << 6) | 0x3F,
            0xFF17 => {
                (self.channel2.envelope.initial_volume << 4)
                    | ((self.channel2.envelope.direction_increase as u8) << 3)
                    | self.channel2.envelope.period
            }
            0xFF18 => 0xFF, // write-only
            0xFF19 => 0xBF | ((self.channel2.length_enabled as u8) << 6),

            // Channel 3
            0xFF1A => 0x7F | ((self.channel3.dac_enabled as u8) << 7),
            0xFF1B => 0xFF, // write-only
            0xFF1C => 0x9F | (self.channel3.volume_code << 5),
            0xFF1D => 0xFF, // write-only
            0xFF1E => 0xBF | ((self.channel3.length_enabled as u8) << 6),

            // Channel 4
            0xFF20 => 0xFF, // write-only
            0xFF21 => {
                (self.channel4.envelope.initial_volume << 4)
                    | ((self.channel4.envelope.direction_increase as u8) << 3)
                    | self.channel4.envelope.period
            }
            0xFF22 => {
                (self.channel4.clock_shift << 4)
                    | ((self.channel4.width_mode_7bit as u8) << 3)
                    | self.channel4.divisor_code
            }
            0xFF23 => 0xBF | ((self.channel4.length_enabled as u8) << 6),

            // Master control
            0xFF24 => self.nr50,
            0xFF25 => self.nr51,
            0xFF26 => {
                let mut status = if self.enabled { 0x80 } else { 0x00 };
                status |= self.channel1.enabled as u8;
                status |= (self.channel2.enabled as u8) << 1;
                status |= (self.channel3.enabled as u8) << 2;
                status |= (self.channel4.enabled as u8) << 3;
                status | 0x70
            }

            // Wave RAM
            0xFF30..=0xFF3F => self.wave_ram[(address - 0xFF30) as usize],

            // Unused sound registers
            _ => 0xFF,
        }
    }

    pub fn write(&mut self, address: u16, value: u8) {
        // Aside from NR52 itself and wave RAM, real hardware ignores
        // writes while the APU is powered off. We follow that here too.
        if !self.enabled && address != 0xFF26 && !(0xFF30..=0xFF3F).contains(&address) {
            return;
        }

        match address {
            // Channel 1
            0xFF10 => {
                self.sweep.period = (value >> 4) & 0x07;
                self.sweep.negate = value & 0x08 != 0;
                self.sweep.shift = value & 0x07;
            }
            0xFF11 => {
                self.channel1.duty = value >> 6;
                self.channel1.length_counter = 64 - (value & 0x3F) as u16;
            }
            0xFF12 => {
                self.channel1.envelope.initial_volume = value >> 4;
                self.channel1.envelope.direction_increase = value & 0x08 != 0;
                self.channel1.envelope.period = value & 0x07;
                self.channel1.dac_enabled = value & 0xF8 != 0;
                if !self.channel1.dac_enabled {
                    self.channel1.enabled = false;
                }
            }
            0xFF13 => {
                self.channel1.frequency = (self.channel1.frequency & 0x0700) | value as u16;
            }
            0xFF14 => {
                self.channel1.frequency =
                    (self.channel1.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.channel1.length_enabled = value & 0x40 != 0;

                if value & 0x80 != 0 {
                    self.channel1.trigger();
                    if !self.sweep.trigger(self.channel1.frequency) {
                        self.channel1.enabled = false;
                    }
                }
            }

            // Channel 2
            0xFF16 => {
                self.channel2.duty = value >> 6;
                self.channel2.length_counter = 64 - (value & 0x3F) as u16;
            }
            0xFF17 => {
                self.channel2.envelope.initial_volume = value >> 4;
                self.channel2.envelope.direction_increase = value & 0x08 != 0;
                self.channel2.envelope.period = value & 0x07;
                self.channel2.dac_enabled = value & 0xF8 != 0;
                if !self.channel2.dac_enabled {
                    self.channel2.enabled = false;
                }
            }
            0xFF18 => {
                self.channel2.frequency = (self.channel2.frequency & 0x0700) | value as u16;
            }
            0xFF19 => {
                self.channel2.frequency =
                    (self.channel2.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.channel2.length_enabled = value & 0x40 != 0;

                if value & 0x80 != 0 {
                    self.channel2.trigger();
                }
            }

            // Channel 3
            0xFF1A => {
                self.channel3.dac_enabled = value & 0x80 != 0;
                if !self.channel3.dac_enabled {
                    self.channel3.enabled = false;
                }
            }
            0xFF1B => {
                self.channel3.length_counter = 256 - value as u16;
            }
            0xFF1C => {
                self.channel3.volume_code = (value >> 5) & 0x03;
            }
            0xFF1D => {
                self.channel3.frequency = (self.channel3.frequency & 0x0700) | value as u16;
            }
            0xFF1E => {
                self.channel3.frequency =
                    (self.channel3.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.channel3.length_enabled = value & 0x40 != 0;

                if value & 0x80 != 0 {
                    self.channel3.trigger();
                }
            }

            // Channel 4
            0xFF20 => {
                self.channel4.length_counter = 64 - (value & 0x3F) as u16;
            }
            0xFF21 => {
                self.channel4.envelope.initial_volume = value >> 4;
                self.channel4.envelope.direction_increase = value & 0x08 != 0;
                self.channel4.envelope.period = value & 0x07;
                self.channel4.dac_enabled = value & 0xF8 != 0;
                if !self.channel4.dac_enabled {
                    self.channel4.enabled = false;
                }
            }
            0xFF22 => {
                self.channel4.clock_shift = value >> 4;
                self.channel4.width_mode_7bit = value & 0x08 != 0;
                self.channel4.divisor_code = value & 0x07;
            }
            0xFF23 => {
                self.channel4.length_enabled = value & 0x40 != 0;

                if value & 0x80 != 0 {
                    self.channel4.trigger();
                }
            }

            // Master volume / panning
            0xFF24 => self.nr50 = value,
            0xFF25 => self.nr51 = value,

            // Master enable
            0xFF26 => {
                let turning_on = value & 0x80 != 0;

                if self.enabled && !turning_on {
                    self.power_off();
                }

                self.enabled = turning_on;
            }

            // Wave RAM - always writable regardless of power state
            0xFF30..=0xFF3F => {
                self.wave_ram[(address - 0xFF30) as usize] = value;
            }

            _ => {}
        }
    }

    // Powering off clears every register except wave RAM - matches real
    // hardware, and conveniently means turning the APU back on always
    // starts from a silent, known state.
    fn power_off(&mut self) {
        self.nr50 = 0;
        self.nr51 = 0;
        self.channel1 = SquareChannel::default();
        self.sweep = Sweep::default();
        self.channel2 = SquareChannel::default();
        self.channel3 = WaveChannel::default();
        self.channel4 = NoiseChannel::default();
    }

    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.tick_one();
        }
    }

    fn tick_one(&mut self) {
        if self.enabled {
            self.channel1.tick();
            self.channel2.tick();
            self.channel3.tick();
            self.channel4.tick();

            self.frame_sequencer_counter += 1;
            if self.frame_sequencer_counter >= 8192 {
                self.frame_sequencer_counter = 0;
                self.step_frame_sequencer();
            }
        }

        self.sample_accumulator += 1.0;
        if self.sample_accumulator >= self.cycles_per_sample {
            self.sample_accumulator -= self.cycles_per_sample;
            self.push_sample();
        }
    }

    fn step_frame_sequencer(&mut self) {
        match self.frame_sequencer_step {
            0 | 4 => self.clock_length(),
            2 | 6 => {
                self.clock_length();
                self.clock_sweep();
            }
            7 => self.clock_envelope(),
            _ => {}
        }

        self.frame_sequencer_step = (self.frame_sequencer_step + 1) & 7;
    }

    fn clock_length(&mut self) {
        self.channel1.clock_length();
        self.channel2.clock_length();
        self.channel3.clock_length();
        self.channel4.clock_length();
    }

    fn clock_envelope(&mut self) {
        self.channel1.envelope.tick();
        self.channel2.envelope.tick();
        self.channel4.envelope.tick();
    }

    fn clock_sweep(&mut self) {
        match self.sweep.tick() {
            SweepResult::NoChange => {}
            SweepResult::UpdateFrequency(freq) => self.channel1.frequency = freq,
            SweepResult::Disable => self.channel1.enabled = false,
        }
    }

    // Mixes the four channels down to a stereo sample and appends it to
    // the output buffer. Each channel's 0-15 amplitude is normalized like
    // a real DMG DAC would (silence sits at 0.0, not at the bottom of the
    // range), panned per NR51, then averaged and scaled by NR50.
    fn push_sample(&mut self) {
        if !self.enabled {
            self.sample_buffer.push(0.0);
            self.sample_buffer.push(0.0);
            return;
        }

        let to_dac = |amplitude: u8| (amplitude as f32 / 7.5) - 1.0;

        let channels = [
            (
                to_dac(self.channel1.amplitude()),
                self.nr51 & 0x10 != 0,
                self.nr51 & 0x01 != 0,
            ),
            (
                to_dac(self.channel2.amplitude()),
                self.nr51 & 0x20 != 0,
                self.nr51 & 0x02 != 0,
            ),
            (
                to_dac(self.channel3.amplitude(&self.wave_ram)),
                self.nr51 & 0x40 != 0,
                self.nr51 & 0x04 != 0,
            ),
            (
                to_dac(self.channel4.amplitude()),
                self.nr51 & 0x80 != 0,
                self.nr51 & 0x08 != 0,
            ),
        ];

        let (mut left, mut right) = (0.0f32, 0.0f32);
        let (mut left_count, mut right_count) = (0u32, 0u32);

        for (sample, on_left, on_right) in channels {
            if on_left {
                left += sample;
                left_count += 1;
            }
            if on_right {
                right += sample;
                right_count += 1;
            }
        }

        if left_count > 0 {
            left /= left_count as f32;
        }
        if right_count > 0 {
            right /= right_count as f32;
        }

        let left_volume = ((self.nr50 >> 4) & 0x07) as f32 / 7.0;
        let right_volume = (self.nr50 & 0x07) as f32 / 7.0;

        self.sample_buffer.push(left * left_volume);
        self.sample_buffer.push(right * right_volume);
    }

    // Hands ownership of every sample generated since the last call to the
    // caller (main.rs, to push into the real audio output stream). Empties
    // the internal buffer.
    pub fn take_samples(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.sample_buffer)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate as u32
    }
}