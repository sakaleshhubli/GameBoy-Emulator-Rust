// Loads a ROM file and handles cartridge memory banking.
//
// Supports: ROM ONLY (no banking), MBC1 (Tetris, Zelda: Link's Awakening,
// Metroid II) and MBC3 (Pokemon Red/Blue/Gold/Silver, Zelda: Link's
// Awakening DX). Cartridge types other than these are detected but not
// yet banked - the ROM loads and the fixed first bank works, but bank
// switching is a no-op, so those games will misbehave past the first
// 32KB. Add a match arm in mbc_type_from_header() when you're ready to
// support MBC2/MBC5.

#[derive(PartialEq, Clone, Copy)]
enum MbcType {
    None,
    Mbc1,
    Mbc3,
}

// MBC3's real-time clock registers. We don't tick these against wall-clock
// time - there's no time source wired in - so they just hold whatever the
// game last wrote via write_ram. That's enough for games that set/read the
// clock without depending on it actually advancing on its own; a real
// implementation would advance `seconds`/`minutes`/etc. based on elapsed
// wall-clock time each time the cartridge is loaded.
#[derive(Default, Clone, Copy)]
struct RtcRegisters {
    seconds: u8,
    minutes: u8,
    hours: u8,
    day_low: u8,
    day_high: u8, // bit 0: day counter bit 8, bit 6: halt, bit 7: day carry

    // Snapshot taken on a latch (see write_mbc3's 0x6000-0x7FFF arm) -
    // 0xA000-0xBFFF reads always come from here, not the live registers,
    // so a game can read a consistent multi-byte timestamp mid-tick.
    latched_seconds: u8,
    latched_minutes: u8,
    latched_hours: u8,
    latched_day_low: u8,
    latched_day_high: u8,
}

impl RtcRegisters {
    fn latch(&mut self) {
        self.latched_seconds = self.seconds;
        self.latched_minutes = self.minutes;
        self.latched_hours = self.hours;
        self.latched_day_low = self.day_low;
        self.latched_day_high = self.day_high;
    }

    fn read(&self, register: u8) -> u8 {
        match register {
            0x08 => self.latched_seconds,
            0x09 => self.latched_minutes,
            0x0A => self.latched_hours,
            0x0B => self.latched_day_low,
            0x0C => self.latched_day_high,
            _ => 0xFF,
        }
    }

    fn write(&mut self, register: u8, value: u8) {
        match register {
            0x08 => self.seconds = value,
            0x09 => self.minutes = value,
            0x0A => self.hours = value,
            0x0B => self.day_low = value,
            0x0C => self.day_high = value,
            _ => {}
        }
    }
}

pub struct Cartridge {
    rom: Vec<u8>,
    ram: Vec<u8>,

    mbc_type: MbcType,

    // How many 16KB banks the ROM has and 8KB banks the RAM has -
    // used to wrap bank numbers so out-of-range writes can't panic.
    rom_bank_count: usize,
    ram_bank_count: usize,

    // Shared by both banking chips: whether external RAM (and, on MBC3,
    // the RTC too) currently responds to reads/writes.
    ram_enabled: bool,

    // MBC1 register state
    rom_bank_low: u8, // 5 bits, written via 0x2000-0x3FFF
    ram_bank: u8,     // 2 bits, written via 0x4000-0x5FFF (also upper ROM bank bits in mode 0)
    banking_mode: u8, // 0 = ROM banking mode, 1 = RAM banking mode, written via 0x6000-0x7FFF

    // MBC3 register state
    mbc3_rom_bank: u8,        // full 7 bits, written via 0x2000-0x3FFF
    mbc3_ram_bank_or_rtc: u8, // 0x00-0x03 selects a RAM bank, 0x08-0x0C selects an RTC register - written via 0x4000-0x5FFF
    mbc3_latch_prev: u8,      // last byte written to 0x6000-0x7FFF, to detect the 0x00->0x01 latch sequence
    rtc: RtcRegisters,
}

impl Cartridge {

    pub fn load(path: &str) -> std::io::Result<Self> {
        let rom = std::fs::read(path)?;

        let mbc_type = Self::mbc_type_from_header(&rom);
        let rom_bank_count = Self::rom_bank_count_from_header(&rom);
        let (ram_size, ram_bank_count) = Self::ram_size_from_header(&rom);

        Ok(Self {
            rom,
            ram: vec![0; ram_size],

            mbc_type,

            rom_bank_count,
            ram_bank_count,

            ram_enabled: false,
            rom_bank_low: 1,
            ram_bank: 0,
            banking_mode: 0,

            mbc3_rom_bank: 1,
            mbc3_ram_bank_or_rtc: 0,
            mbc3_latch_prev: 0xFF, // not 0x00, so a stray first write can't accidentally latch
            rtc: RtcRegisters::default(),
        })
    }

    // Cartridge header byte 0x0147 - what banking hardware is on the cartridge
    fn mbc_type_from_header(rom: &[u8]) -> MbcType {
        match rom.get(0x0147).copied().unwrap_or(0x00) {
            0x00 => MbcType::None,
            0x01..=0x03 => MbcType::Mbc1,
            0x0F..=0x13 => MbcType::Mbc3,
            other => {
                println!(
                    "Cartridge type {:#04X} isn't banked yet - only the fixed \
                     first ROM bank will be readable.",
                    other
                );
                MbcType::None
            }
        }
    }

    // Cartridge header byte 0x0148 - ROM size, encoded as 32KB << n
    fn rom_bank_count_from_header(rom: &[u8]) -> usize {
        let code = rom.get(0x0148).copied().unwrap_or(0x00);
        2usize << code // (32KB << code) / 16KB per bank
    }

    // Cartridge header byte 0x0149 - external RAM size
    // Returns (total bytes, number of 8KB banks)
    fn ram_size_from_header(rom: &[u8]) -> (usize, usize) {
        match rom.get(0x0149).copied().unwrap_or(0x00) {
            0x00 => (0, 0),
            0x01 => (0x800, 1),       // 2 KB, unofficial - treated as a single unbanked chunk
            0x02 => (0x2000, 1),      // 8 KB
            0x03 => (0x8000, 4),      // 32 KB - 4 banks
            0x04 => (0x20000, 16),    // 128 KB - 16 banks
            0x05 => (0x10000, 8),     // 64 KB - 8 banks
            _ => (0, 0),
        }
    }

    // Resolves the current ROM bank register(s) into the actual bank
    // number to read from, per banking chip. Handles the "register value
    // 0 reads as 1" quirk both chips share.
    fn current_rom_bank(&self) -> usize {

        let bank = match self.mbc_type {
            MbcType::None => return 1,

            MbcType::Mbc1 => {
                let mut low = self.rom_bank_low & 0x1F;
                if low == 0 {
                    low = 1;
                }

                if self.banking_mode == 0 {
                    ((self.ram_bank as usize) << 5) | low as usize
                } else {
                    low as usize
                }
            }

            // MBC3 has one flat 7-bit bank register - no low/high split
            // and no separate banking mode to worry about.
            MbcType::Mbc3 => {
                let bank = self.mbc3_rom_bank & 0x7F;
                if bank == 0 { 1 } else { bank as usize }
            }
        };

        if self.rom_bank_count == 0 {
            bank
        } else {
            bank % self.rom_bank_count
        }
    }

    // In RAM banking mode, ram_bank selects the active 8KB RAM bank.
    // In ROM banking mode, RAM is fixed to bank 0.
    fn current_ram_bank(&self) -> usize {
        match self.mbc_type {
            MbcType::Mbc1 if self.banking_mode == 1 => self.ram_bank as usize,
            MbcType::Mbc3 => (self.mbc3_ram_bank_or_rtc & 0x03) as usize,
            _ => 0,
        }
    }

    // Reads a byte from ROM. address is 0x0000-0x7FFF as seen by the CPU.
    pub fn read(&self, address: u16) -> u8 {
        match address {

            // 0x0000-0x3FFF - fixed bank 0
            0x0000..=0x3FFF => self.rom.get(address as usize).copied().unwrap_or(0xFF),

            // 0x4000-0x7FFF - switchable bank, selected by the MBC registers
            0x4000..=0x7FFF => {
                let bank = self.current_rom_bank();
                let offset = bank * 0x4000 + (address - 0x4000) as usize;
                self.rom.get(offset).copied().unwrap_or(0xFF)
            }

            _ => 0xFF,
        }
    }

    // Handles writes to 0x0000-0x7FFF, which on a banked cartridge don't
    // touch ROM at all - they're how the game talks to the MBC's registers.
    pub fn write(&mut self, address: u16, value: u8) {
        match self.mbc_type {
            MbcType::None => {} // no banking hardware, nothing to configure
            MbcType::Mbc1 => self.write_mbc1(address, value),
            MbcType::Mbc3 => self.write_mbc3(address, value),
        }
    }

    fn write_mbc1(&mut self, address: u16, value: u8) {
        match address {

            // 0x0000-0x1FFF - RAM enable. Writing 0x0A in the low nibble
            // enables external RAM; any other value disables it.
            0x0000..=0x1FFF => {
                self.ram_enabled = (value & 0x0F) == 0x0A;
            }

            // 0x2000-0x3FFF - lower 5 bits of the ROM bank number
            0x2000..=0x3FFF => {
                self.rom_bank_low = value & 0x1F;
            }

            // 0x4000-0x5FFF - RAM bank number, or the upper 2 bits of the
            // ROM bank number depending on banking_mode
            0x4000..=0x5FFF => {
                self.ram_bank = value & 0x03;
            }

            // 0x6000-0x7FFF - banking mode select
            0x6000..=0x7FFF => {
                self.banking_mode = value & 0x01;
            }

            _ => {}
        }
    }

    fn write_mbc3(&mut self, address: u16, value: u8) {
        match address {

            // 0x0000-0x1FFF - RAM & RTC enable, same 0x0A-in-low-nibble
            // convention as MBC1
            0x0000..=0x1FFF => {
                self.ram_enabled = (value & 0x0F) == 0x0A;
            }

            // 0x2000-0x3FFF - full 7-bit ROM bank number (no low/high
            // split like MBC1 - one register covers the whole range)
            0x2000..=0x3FFF => {
                self.mbc3_rom_bank = value & 0x7F;
            }

            // 0x4000-0x5FFF - RAM bank (0x00-0x03) or RTC register select
            // (0x08-0x0C); which one 0xA000-0xBFFF maps to
            0x4000..=0x5FFF => {
                self.mbc3_ram_bank_or_rtc = value;
            }

            // 0x6000-0x7FFF - latch clock data: writing 0x00 then 0x01
            // copies the live RTC registers into the latched copies that
            // 0xA000-0xBFFF actually reads from
            0x6000..=0x7FFF => {
                if self.mbc3_latch_prev == 0x00 && value == 0x01 {
                    self.rtc.latch();
                }
                self.mbc3_latch_prev = value;
            }

            _ => {}
        }
    }

    // Reads a byte from external cartridge RAM. address is 0xA000-0xBFFF
    // as seen by the CPU. Returns open-bus 0xFF if RAM is disabled/absent.
    pub fn read_ram(&self, address: u16) -> u8 {

        if !self.ram_enabled {
            return 0xFF;
        }

        if self.mbc_type == MbcType::Mbc3 && self.mbc3_ram_bank_or_rtc >= 0x08 {
            return self.rtc.read(self.mbc3_ram_bank_or_rtc);
        }

        if self.ram.is_empty() {
            return 0xFF;
        }

        let bank = self.current_ram_bank();
        let offset = bank * 0x2000 + (address - 0xA000) as usize;

        self.ram.get(offset).copied().unwrap_or(0xFF)
    }

    // Writes a byte to external cartridge RAM. Silently ignored if RAM is
    // disabled or the cartridge has none.
    pub fn write_ram(&mut self, address: u16, value: u8) {

        if !self.ram_enabled {
            return;
        }

        if self.mbc_type == MbcType::Mbc3 && self.mbc3_ram_bank_or_rtc >= 0x08 {
            self.rtc.write(self.mbc3_ram_bank_or_rtc, value);
            return;
        }

        if self.ram.is_empty() {
            return;
        }

        let bank = self.current_ram_bank();
        let offset = bank * 0x2000 + (address - 0xA000) as usize;

        if let Some(slot) = self.ram.get_mut(offset) {
            *slot = value;
        }
    }
}