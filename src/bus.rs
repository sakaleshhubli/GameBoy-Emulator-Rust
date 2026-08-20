use crate::cartridge::Cartridge;
use crate::ppu::Ppu;
use crate::joypad::{Button, Joypad};
use crate::sound::Sound;

// The Bus acts as the communication bridge between the CPU and all Game Boy hardware components
pub struct Bus {

    // Game cartridge containing the ROM data
    cartridge: Cartridge,

    // Picture Processing Unit - owns VRAM/OAM and the LCDC/STAT/SCX/SCY/
    // LY/LYC/BGP/OBP0/OBP1/WY/WX registers (0xFF40-0xFF4B)
    ppu: Ppu,

    // Game Boy joypad/controller
    // Connected to memory-mapped register 0xFF00
    joypad: Joypad,

    //Game Boy sound
    sound: Sound,

    // Working RAM (8 KB)
    // Used by programs to store variables and temporary data
    wram: [u8; 0x2000],

    // I/O registers (0xFF00-0xFF7F): joypad, timer, sound, LCD control, etc.
    // Not individually modeled yet - PPU/timer milestones will read/write
    // specific offsets here. IF (0xFF0F) is pulled out as its own field below
    // since the CPU touches it every step.
    io: [u8; 0x80],

    // High RAM (127 bytes)
    // Small, fast memory used by the CPU
    hram: [u8; 0x7F],

    // Timer registers (0xFF04-0xFF07)
    // div_counter is the real 16-bit internal counter; the visible DIV
    // register (0xFF04) is just its upper 8 bits, incrementing every 256
    // T-cycles (16384 Hz). Writing any value to DIV resets it to 0.
    div_counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,


    // Counts T-cycles toward the next TIMA increment. Threshold depends
    // on TAC's clock-select bits (see tick_one). This is a simplification
    // of real hardware, which derives TIMA's clock from a specific bit of
    // div_counter - good enough for normal timer use, but won't reproduce
    // the DIV-write glitch that some advanced timer test ROMs check for.
    timer_subcounter: u32,

    // Interrupt Flag (0xFF0F) - which interrupts are currently pending
    pub interrupt_flag: u8,

    // Interrupt Enable (0xFFFF) - which interrupts the program has enabled
    pub interrupt_enable: u8,
}

impl Bus {

    // Creates a new memory bus and initializes RAM. sample_rate is the
    // host audio device's actual output rate - the Sound module generates
    // samples at exactly that rate so main.rs can hand them to cpal
    // without any resampling.
    pub fn new(cartridge: Cartridge, sample_rate: u32) -> Self {
        Self {

            // Store the loaded cartridge
            cartridge,

            ppu: Ppu::new(),

            joypad: Joypad::new(),

            sound: Sound::new(sample_rate),

            // Initialize Working RAM with zeros
            wram: [0; 0x2000],

            io: [0; 0x80],

            // Initialize High RAM with zeros
            hram: [0; 0x7F],

            div_counter: 0,
            tima: 0,
            tma: 0,
            tac: 0,
            timer_subcounter: 0,

            interrupt_flag: 0,
            interrupt_enable: 0,
        }
    }

    // Reads one byte from the specified memory address
    pub fn read_byte(&self, address: u16) -> u8 {

        // Determine which memory region the address belongs to
        match address {

            // 0x0000 - 0x7FFF
            // Read data directly from the cartridge ROM
            0x0000..=0x7FFF => self.cartridge.read(address),

            // 0x8000 - 0x9FFF
            // Read from Video RAM (gated by the PPU's current mode)
            0x8000..=0x9FFF => self.ppu.read_vram(address),

            // 0xA000 - 0xBFFF
            // Cartridge external RAM, banked by the MBC
            0xA000..=0xBFFF => self.cartridge.read_ram(address),

            // 0xC000 - 0xDFFF
            // Read from Working RAM
            0xC000..=0xDFFF => {
                self.wram[(address - 0xC000) as usize]
            }

            // 0xE000 - 0xFDFF
            // Echo RAM - hardware mirrors this straight back onto WRAM
            0xE000..=0xFDFF => {
                self.wram[(address - 0xE000) as usize]
            }

            // 0xFE00 - 0xFE9F
            // Read from Object Attribute Memory (sprite table, gated by
            // the PPU's current mode)
            0xFE00..=0xFE9F => self.ppu.read_oam(address),

            // 0xFEA0 - 0xFEFF
            // Unusable region on real hardware
            0xFEA0..=0xFEFF => 0xFF,

            // 0xFF40 - 0xFF4B
            // PPU registers: LCDC, STAT, SCY, SCX, LY, LYC, BGP, OBP0,
            // OBP1, WY, WX. 0xFF46 (DMA) isn't a PPU register - it just
            // falls through to the generic I/O read below.
            0xFF40..=0xFF45 | 0xFF47..=0xFF4B => self.ppu.read_register(address),

            // 0xFF04
            // DIV - visible register is just the counter's upper byte
            0xFF04 => (self.div_counter >> 8) as u8,

            // 0xFF05
            // TIMA - the counter that actually generates the Timer interrupt
            0xFF05 => self.tima,

            // 0xFF06
            // TMA - value TIMA reloads with on overflow
            0xFF06 => self.tma,

            // 0xFF07
            // TAC - only the low 3 bits are meaningful, the rest read as 1
            0xFF07 => self.tac | 0xF8,

            // 0xFF0F
            // Interrupt Flag register
            0xFF0F => self.interrupt_flag,

            // 0xFF00
            // Joypad register
            0xFF00 => self.joypad.read(),

            0xFF10..=0xFF26 => self.sound.read(address),

            // 0xFF30 - 0xFF3F
            // Wave RAM - channel 3's 32 4-bit samples
            0xFF30..=0xFF3F => self.sound.read(address),

            // 0xFF00 - 0xFF7F
            // I/O registers
            0xFF00..=0xFF7F => {
                self.io[(address - 0xFF00) as usize]
            }

            // 0xFF80 - 0xFFFE
            // Read from High RAM
            0xFF80..=0xFFFE => {
                self.hram[(address - 0xFF80) as usize]
            }

            // 0xFFFF
            // Interrupt Enable register
            0xFFFF => self.interrupt_enable,
        }
    }

    // Writes one byte to the specified memory address
    pub fn write_byte(&mut self, address: u16, value: u8) {

        // Determine which memory region the address belongs to
        match address {

            // 0x0000 - 0x7FFF
            // Writes here don't touch ROM - they configure the cartridge's
            // MBC registers (RAM enable, bank number, banking mode)
            0x0000..=0x7FFF => {
                self.cartridge.write(address, value);
            }

            // 0x8000 - 0x9FFF
            // Write to Video RAM (gated by the PPU's current mode)
            0x8000..=0x9FFF => self.ppu.write_vram(address, value),

            // 0xA000 - 0xBFFF
            // Cartridge external RAM, banked by the MBC
            0xA000..=0xBFFF => {
                self.cartridge.write_ram(address, value);
            }

            // 0xC000 - 0xDFFF
            // Store the value in Working RAM
            0xC000..=0xDFFF => {
                self.wram[(address - 0xC000) as usize] = value;
            }

            // 0xE000 - 0xFDFF
            // Echo RAM mirrors WRAM
            0xE000..=0xFDFF => {
                self.wram[(address - 0xE000) as usize] = value;
            }

            // 0xFE00 - 0xFE9F
            // Write to Object Attribute Memory (gated by the PPU's
            // current mode)
            0xFE00..=0xFE9F => self.ppu.write_oam(address, value),

            // Unusable region
            0xFEA0..=0xFEFF => {}

            // 0xFF40 - 0xFF45, 0xFF47 - 0xFF4B
            // PPU registers (0xFF46 is DMA, handled separately below)
            0xFF40..=0xFF45 | 0xFF47..=0xFF4B => {
                self.ppu.write_register(address, value);
            }

            // 0xFF46
            // OAM DMA. Real hardware copies 160 bytes from
            // (value << 8)..+0xA0 into OAM over 160 M-cycles, during which
            // the CPU can only access HRAM. We do the copy instantly
            // instead of modeling that timing/lockout - a common
            // simplification that's fine for anything that isn't timing
            // its DMA kicks against CPU execution.
            0xFF46 => {
                let source_base = (value as u16) << 8;
                for i in 0..0xA0u16 {
                    let byte = self.read_byte(source_base + i);
                    self.ppu.dma_write_oam(i as u8, byte);
                }
            }

            // 0xFF04
            // DIV - writing any value resets the whole internal counter
            0xFF04 => {
                self.div_counter = 0;
            }

            // 0xFF05
            // TIMA
            0xFF05 => {
                self.tima = value;
            }

            // 0xFF06
            // TMA
            0xFF06 => {
                self.tma = value;
            }

            // 0xFF07
            // TAC - only bits 0-2 exist
            0xFF07 => {
                self.tac = value & 0x07;
            }

            // 0xFF0F
            // Interrupt Flag register
            0xFF0F => {
                self.interrupt_flag = value;
            }

            0xFF10..=0xFF26 => {
                self.sound.write(address, value);
            }

            // 0xFF30 - 0xFF3F
            // Wave RAM
            0xFF30..=0xFF3F => {
                self.sound.write(address, value);
            }

            // 0xFF02
            // Serial Transfer Control. Real hardware would shift the byte
            // in 0xFF01 out over the link cable. We don't have a link cable
            // (or a PPU yet to show anything on-screen), so as a headless
            // testing trick: whenever a transfer with the internal clock
            // starts (bit 7 and bit 0 both set), print the pending byte to
            // stdout instead. This is exactly how Blargg's test ROMs report
            // PASS/FAIL/diagnostic text, so it's a real testing tool, not
            // just a hack - rip it out once the PPU can render text itself.
            0xFF02 => {
                self.io[(address - 0xFF00) as usize] = value;

                if value == 0x81 {
                    let byte = self.io[(0xFF01 - 0xFF00) as usize];
                    print!("{}", byte as char);
                    use std::io::Write;
                    std::io::stdout().flush().ok();

                    // Clear the control register so this transfer doesn't
                    // trigger again next time this address is written
                    self.io[(0xFF02 - 0xFF00) as usize] = 0;
                }
            }

            // 0xFF00
            // Joypad register
            0xFF00 => {
                self.joypad.write(value);
            }

            // 0xFF00 - 0xFF7F
            // I/O registers
            0xFF00..=0xFF7F => {
                self.io[(address - 0xFF00) as usize] = value;
            }

            // 0xFF80 - 0xFFFE
            // Store the value in High RAM
            0xFF80..=0xFFFE => {
                self.hram[(address - 0xFF80) as usize] = value;
            }

            // 0xFFFF
            // Interrupt Enable register
            0xFFFF => {
                self.interrupt_enable = value;
            }
        }
    }

    // Sets an interrupt's pending bit in IF. Call this from the PPU/timer/
    // joypad/serial once those exist, e.g. bus.request_interrupt(0) on VBlank.
    // Bits: 0=VBlank 1=LCD STAT 2=Timer 3=Serial 4=Joypad
    pub fn request_interrupt(&mut self, bit: u8) {
        self.interrupt_flag |= 1 << bit;
    }

    // Advances DIV and (if enabled) TIMA by the given number of T-cycles.
    // Called once per CPU step with that instruction's cycle cost.
    pub fn tick(&mut self, cycles: u32) {
        for _ in 0..cycles {
            self.tick_one();
        }

        let ppu_interrupts = self.ppu.tick(cycles);
        if ppu_interrupts.vblank {
            self.request_interrupt(0);
        }
        if ppu_interrupts.stat {
            self.request_interrupt(1);
        }

        self.sound.tick(cycles);
    }

    // Exposes the current frame for a display layer to draw. See
    // Ppu::framebuffer for what the shade values mean.
    pub fn framebuffer(&self) -> &[u8; crate::ppu::SCREEN_WIDTH * crate::ppu::SCREEN_HEIGHT] {
        self.ppu.framebuffer()
    }

    // Hands over every audio sample generated since the last call, as
    // interleaved stereo f32s ready to feed straight into a cpal stream.
    pub fn take_audio_samples(&mut self) -> Vec<f32> {
        self.sound.take_samples()
    }

    // Advances the timer hardware by a single T-cycle. Looping this from
    // tick() (rather than doing the whole batch in one division) keeps the
    // TIMA-overflow check correct even when a single instruction's cycle
    // count spans more than one TIMA increment.
    fn tick_one(&mut self) {
        self.div_counter = self.div_counter.wrapping_add(1);

        // Timer enable is TAC bit 2
        if self.tac & 0x04 == 0 {
            return;
        }

        // TAC bits 0-1 select TIMA's increment frequency
        let threshold = match self.tac & 0x03 {
            0 => 1024, // 4096 Hz
            1 => 16,   // 262144 Hz
            2 => 64,   // 65536 Hz
            3 => 256,  // 16384 Hz
            _ => unreachable!(),
        };

        self.timer_subcounter += 1;

        if self.timer_subcounter >= threshold {
            self.timer_subcounter = 0;

            self.tima = self.tima.wrapping_add(1);

            if self.tima == 0 {
                // Overflow - reload from TMA and request the Timer interrupt
                self.tima = self.tma;
                self.request_interrupt(2);
            }
        }
    }

    // Requests the joypad interrupt only on an actual press edge - not
    // every frame a key happens to still be held down (see Joypad::press).
    pub fn press_button(&mut self, button: Button) {
        if self.joypad.press(button) {
            self.request_interrupt(4);
        }
    }

    pub fn release_button(&mut self, button: Button) {
        self.joypad.release(button);
    }
}