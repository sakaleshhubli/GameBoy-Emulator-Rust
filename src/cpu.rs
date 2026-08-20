use crate::bus::Bus;

// Standard Game Boy T-cycle cost for each unprefixed opcode. Conditional
// jump/call/ret entries hold the "not taken" cost - the extra cycles for
// a taken branch are added via branch_extra_cycles in the relevant arm.
// Unused/illegal opcodes (which real hardware locks up on) are given a
// placeholder of 4 since execute()'s fallback arm never advances PC further.
const CYCLES: [u8; 256] = [
    // 0x00-0x0F
    4, 12, 8, 8, 4, 4, 8, 4, 20, 8, 8, 8, 4, 4, 8, 4,
    // 0x10-0x1F
    4, 12, 8, 8, 4, 4, 8, 4, 12, 8, 8, 8, 4, 4, 8, 4,
    // 0x20-0x2F
    8, 12, 8, 8, 4, 4, 8, 4, 8, 8, 8, 8, 4, 4, 8, 4,
    // 0x30-0x3F
    8, 12, 8, 8, 12, 12, 12, 4, 8, 8, 8, 8, 4, 4, 8, 4,
    // 0x40-0x4F
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0x50-0x5F
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0x60-0x6F
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0x70-0x7F
    8, 8, 8, 8, 8, 8, 4, 8, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0x80-0x8F
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0x90-0x9F
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0xA0-0xAF
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0xB0-0xBF
    4, 4, 4, 4, 4, 4, 8, 4, 4, 4, 4, 4, 4, 4, 8, 4,
    // 0xC0-0xCF
    8, 12, 12, 16, 12, 16, 8, 16, 8, 16, 12, 4, 12, 24, 8, 16,
    // 0xD0-0xDF
    8, 12, 12, 4, 12, 16, 8, 16, 8, 16, 12, 4, 12, 4, 8, 16,
    // 0xE0-0xEF
    12, 12, 8, 4, 4, 16, 8, 16, 16, 4, 16, 4, 4, 4, 8, 16,
    // 0xF0-0xFF
    12, 12, 8, 4, 4, 16, 8, 16, 12, 8, 16, 4, 4, 4, 8, 16,
];

pub struct Cpu {
    // CPU registers
    // Each register stores 8-bit values
    a: u8,
    f: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    h: u8,
    l: u8,

    // Program Counter
    // Stores address of next instruction
    pc: u16,

    // Stack Pointer
    // Points to current location of stack
    sp: u16,

    // Set by HALT (0x76); CPU stops fetching until an interrupt wakes it.
    halted: bool,

    // Interrupt Master Enable - global switch for all interrupts
    ime: bool,

    // EI takes effect after the instruction *following* EI finishes, not
    // immediately. This counts down 2 -> 1 -> 0; ime is set true at 0.
    ime_delay: u8,

    // Conditional jump/call/ret cost extra cycles when the branch is taken.
    // Set inside the relevant match arm, added to the base cost from the
    // CYCLES table, then reset to 0 at the top of every execute() call.
    branch_extra_cycles: u8,

    // Connection between CPU and memory
    bus: Bus,
}

impl Cpu {

    pub fn bus_mut(&mut self) -> &mut Bus {
        &mut self.bus
    }

    pub fn new(bus: Bus) -> Self {
        Self {

            // Registers start with Game Boy boot values
            a: 0x01,
            f: 0xB0,

            b: 0x00,
            c: 0x13,

            d: 0x00,
            e: 0xD8,

            h: 0x01,
            l: 0x4D,

            // Game Boy program starts execution at 0x0100
            pc: 0x0100,

            // Initial stack pointer
            sp: 0xFFFE,

            halted: false,
            ime: false,
            ime_delay: 0,
            branch_extra_cycles: 0,

            bus,
        }
    }


    // Executes one CPU cycle. Returns the number of T-cycles it consumed,
    // so callers (e.g. main's frame loop) can pace themselves against the
    // real hardware's clock instead of running flat-out.
    pub fn step(&mut self) -> u32 {

        // Apply a pending EI (see ime_delay doc comment on the struct)
        if self.ime_delay > 0 {
            self.ime_delay -= 1;
            if self.ime_delay == 0 {
                self.ime = true;
            }
        }

        // Service any pending interrupt before fetching the next instruction.
        // This also wakes the CPU from HALT even when IME is off.
        // Dispatch itself costs 20 T-cycles (5 M-cycles) on real hardware.
        if self.handle_interrupts() {
            self.bus.tick(20);
            return 20;
        }

        // HALT: keep idling until an interrupt (handled above) wakes us.
        // The timer/DIV still need to advance while halted, or nothing
        // would ever be able to wake the CPU up - tick one M-cycle per
        // idle step, same granularity a real fetch would use.
        if self.halted {
            self.bus.tick(4);
            return 4;
        }

        // Fetch instruction from memory
        let opcode = self.fetch_byte();

        // Decode and execute instruction, then advance the timer/DIV by
        // however many T-cycles that instruction actually took
        let cycles = self.execute(opcode);
        self.bus.tick(cycles as u32);
        cycles as u32
    }

    // Read-only access to the bus, e.g. so a display loop can pull the
    // PPU's finished framebuffer without the CPU having to know displays
    // exist.
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    // Checks IF & IE for a pending, enabled interrupt and dispatches to its
    // vector if IME is set. Returns true if an interrupt was serviced (in
    // which case that "step" was the dispatch, not a normal instruction).
    fn handle_interrupts(&mut self) -> bool {

        let pending = self.bus.interrupt_flag & self.bus.interrupt_enable & 0x1F;

        if !self.ime {
            // A pending interrupt still wakes the CPU out of HALT even
            // while interrupts are globally disabled.
            if self.halted && pending != 0 {
                self.halted = false;
            }
            return false;
        }

        if pending == 0 {
            return false;
        }

        self.halted = false;
        self.ime = false;

        // Priority by bit order: 0 VBlank, 1 LCD STAT, 2 Timer, 3 Serial, 4 Joypad
        let bit = pending.trailing_zeros() as u8;
        self.bus.interrupt_flag &= !(1 << bit);

        let vector = match bit {
            0 => 0x40, // VBlank
            1 => 0x48, // LCD STAT
            2 => 0x50, // Timer
            3 => 0x58, // Serial
            4 => 0x60, // Joypad
            _ => unreachable!(),
        };

        self.push_word(self.pc);
        self.pc = vector;

        true
    }


    // Reads the next byte from memory
    fn fetch_byte(&mut self) -> u8 {

        let opcode = self.bus.read_byte(self.pc);

        // Move to the next byte in memory
        self.pc += 1;

        opcode
    }

    // Reads the next two bytes from memory (little-endian)
    fn fetch_word(&mut self) -> u16 {

        let low = self.fetch_byte() as u16;
        let high = self.fetch_byte() as u16;

        (high << 8) | low
    }

    // Reads an 8-bit register by its 3-bit opcode index.
    // GB opcodes encode registers as: 0=B 1=C 2=D 3=E 4=H 5=L 6=(HL) 7=A
    fn get_r8(&mut self, idx: u8) -> u8 {
        match idx {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => self.h,
            5 => self.l,
            6 => self.bus.read_byte(self.get_hl()),
            7 => self.a,
            _ => unreachable!(),
        }
    }

    // Writes an 8-bit register by its 3-bit opcode index. See get_r8.
    fn set_r8(&mut self, idx: u8, value: u8) {
        match idx {
            0 => self.b = value,
            1 => self.c = value,
            2 => self.d = value,
            3 => self.e = value,
            4 => self.h = value,
            5 => self.l = value,
            6 => self.bus.write_byte(self.get_hl(), value),
            7 => self.a = value,
            _ => unreachable!(),
        }
    }


    // Executes instructions. Returns the number of T-cycles it took, used
    // by step() to advance the timer/DIV by the right amount.
    fn execute(&mut self, opcode: u8) -> u8 {

        self.branch_extra_cycles = 0;

        // CB-prefixed instructions have their own cycle costs (8/12/16
        // depending on operand and group) that don't fit the CYCLES table,
        // so they're dispatched here and returned directly.
        if opcode == 0xCB {
            let cb_opcode = self.fetch_byte();
            return self.execute_cb(cb_opcode);
        }

        match opcode {

            // 00 - NOP
            0x00 => {}

            // 10 - STOP
            // Followed by a padding byte (usually 0x00); low-power mode
            // not modeled yet, treated as a two-byte NOP for now.
            0x10 => {
                self.fetch_byte();
            }

            // 3E nn
            // Load immediate value into register A
            0x3E => {
                self.a = self.fetch_byte();
            }

            // 06 nn
            // Load immediate value into register B
            0x06 => {
                self.b = self.fetch_byte();
            }

            // 0E nn
            // Load immediate value into register C
            0x0E => {
                self.c = self.fetch_byte();
            }

            // 16 nn
            // Load immediate value into register D
            0x16 => {
                self.d = self.fetch_byte();
            }

            // 1E nn
            // Load immediate value into register E
            0x1E => {
                self.e = self.fetch_byte();
            }

            // 26 nn
            // Load immediate value into register H
            0x26 => {
                self.h = self.fetch_byte();
            }

            // 2E nn
            // Load immediate value into register L
            0x2E => {
                self.l = self.fetch_byte();
            }

            // 36 nn
            // Load immediate value into memory at (HL)
            0x36 => {
                let value = self.fetch_byte();
                self.set_r8(6, value);
            }

            // Non-prefixed accumulator rotates. These reuse the same rotate
            // logic as their CB-prefixed counterparts (rlc/rrc/rl/rr below),
            // but on real hardware always clear the Zero flag regardless of
            // the result - unlike RLC A/RRC A/RL A/RR A via the CB table,
            // which set Zero normally. That's the one difference, so we
            // call the shared helper and then override the flag after.
            0x07 => {
                let a = self.a;
                self.a = self.rlc(a);
                self.set_zero_flag(false);
            }
            0x0F => {
                let a = self.a;
                self.a = self.rrc(a);
                self.set_zero_flag(false);
            }
            0x17 => {
                let a = self.a;
                self.a = self.rl(a);
                self.set_zero_flag(false);
            }
            0x1F => {
                let a = self.a;
                self.a = self.rr(a);
                self.set_zero_flag(false);
            }

            // Indirect/direct-address load instructions - these don't fit
            // the 3-bit register-index scheme used by 0x40-0xBF below, so
            // they're handled as individual opcodes.

            // LD (BC),A / LD (DE),A
            0x02 => self.bus.write_byte(self.get_bc(), self.a),
            0x12 => self.bus.write_byte(self.get_de(), self.a),

            // LD A,(BC) / LD A,(DE)
            0x0A => self.a = self.bus.read_byte(self.get_bc()),
            0x1A => self.a = self.bus.read_byte(self.get_de()),

            // LD (HL+),A / LD (HL-),A - store A then inc/dec HL (LDI/LDD)
            0x22 => {
                let hl = self.get_hl();
                self.bus.write_byte(hl, self.a);
                self.set_hl(hl.wrapping_add(1));
            }
            0x32 => {
                let hl = self.get_hl();
                self.bus.write_byte(hl, self.a);
                self.set_hl(hl.wrapping_sub(1));
            }

            // LD A,(HL+) / LD A,(HL-) - load A then inc/dec HL (LDI/LDD)
            0x2A => {
                let hl = self.get_hl();
                self.a = self.bus.read_byte(hl);
                self.set_hl(hl.wrapping_add(1));
            }
            0x3A => {
                let hl = self.get_hl();
                self.a = self.bus.read_byte(hl);
                self.set_hl(hl.wrapping_sub(1));
            }

            // LD (nn),SP - store SP (little-endian) at an absolute address
            0x08 => {
                let address = self.fetch_word();
                let sp = self.sp;
                self.bus.write_byte(address, sp as u8);
                self.bus.write_byte(address.wrapping_add(1), (sp >> 8) as u8);
            }

            // LDH (n),A / LDH A,(n) - high-page I/O access, 0xFF00 + n
            0xE0 => {
                let n = self.fetch_byte();
                self.bus.write_byte(0xFF00 + n as u16, self.a);
            }
            0xF0 => {
                let n = self.fetch_byte();
                self.a = self.bus.read_byte(0xFF00 + n as u16);
            }

            // LD (C),A / LD A,(C) - high-page I/O access via register C
            0xE2 => self.bus.write_byte(0xFF00 + self.c as u16, self.a),
            0xF2 => self.a = self.bus.read_byte(0xFF00 + self.c as u16),

            // LD (nn),A / LD A,(nn) - absolute direct addressing
            0xEA => {
                let address = self.fetch_word();
                self.bus.write_byte(address, self.a);
            }
            0xFA => {
                let address = self.fetch_word();
                self.a = self.bus.read_byte(address);
            }

            // LD SP,HL
            0xF9 => self.sp = self.get_hl(),

            // ADD SP,e - signed 8-bit offset, flags computed as unsigned
            // byte addition (this is a real GB quirk, not a bug)
            0xE8 => {
                let offset = self.fetch_signed_byte();
                self.sp = self.sp_plus_signed(offset);
            }

            // LD HL,SP+e - same addition as ADD SP,e, result goes to HL,
            // SP itself is untouched
            0xF8 => {
                let offset = self.fetch_signed_byte();
                let result = self.sp_plus_signed(offset);
                self.set_hl(result);
            }

            // Register-to-register and register-to-memory load instructions
            // Covers the entire 0x40-0x7F block: LD r,r' and LD r,(HL) / LD (HL),r
            // 0x76 is the one exception in this block - HALT, not LD (HL),(HL)
            0x40..=0x7F => {
                if opcode == 0x76 {
                    self.halted = true;
                } else {
                    let dest = (opcode >> 3) & 0x07;
                    let src = opcode & 0x07;
                    let value = self.get_r8(src);
                    self.set_r8(dest, value);
                }
            }

            // 16-bit immediate load instructions
            // Load a 16-bit value into a register pair or the Stack Pointer

            // 01 nn nn
            // Load 16-bit immediate value into BC
            0x01 => {
                let value = self.fetch_word();
                self.set_bc(value);
            }

            // 11 nn nn
            // Load 16-bit immediate value into DE
            0x11 => {
                let value = self.fetch_word();
                self.set_de(value);
            }

            // 21 nn nn
            // Load 16-bit immediate value into HL
            0x21 => {
                let value = self.fetch_word();
                self.set_hl(value);
            }

            // 31 nn nn
            // Load 16-bit immediate value into SP
            0x31 => {
                self.sp = self.fetch_word();
            }

            // 8-bit increment and decrement instructions
            // Update the register value and CPU flags
            // 3C - INC A
            0x3C => {
                self.a = self.inc(self.a);
            }

            // 04 - INC B
            0x04 => {
                self.b = self.inc(self.b);
            }

            // 0C - INC C
            0x0C => {
                self.c = self.inc(self.c);
            }

            // 14 - INC D
            0x14 => {
                self.d = self.inc(self.d);
            }

            // 1C - INC E
            0x1C => {
                self.e = self.inc(self.e);
            }

            // 24 - INC H
            0x24 => {
                self.h = self.inc(self.h);
            }

            // 2C - INC L
            0x2C => {
                self.l = self.inc(self.l);
            }

            // 34 - INC (HL)
            0x34 => {
                let value = self.get_r8(6);
                let result = self.inc(value);
                self.set_r8(6, result);
            }

            // 3D - DEC A
            0x3D => {
                self.a = self.dec(self.a);
            }

            // 05 - DEC B
            0x05 => {
                self.b = self.dec(self.b);
            }

            // 0D - DEC C
            0x0D => {
                self.c = self.dec(self.c);
            }

            // 15 - DEC D
            0x15 => {
                self.d = self.dec(self.d);
            }

            // 1D - DEC E
            0x1D => {
                self.e = self.dec(self.e);
            }

            // 25 - DEC H
            0x25 => {
                self.h = self.dec(self.h);
            }

            // 2D - DEC L
            0x2D => {
                self.l = self.dec(self.l);
            }

            // 35 - DEC (HL)
            0x35 => {
                let value = self.get_r8(6);
                let result = self.dec(value);
                self.set_r8(6, result);
            }

            // 16-bit increment and decrement instructions (register pairs, no flags affected)
            0x03 => self.set_bc(self.get_bc().wrapping_add(1)),
            0x13 => self.set_de(self.get_de().wrapping_add(1)),
            0x23 => self.set_hl(self.get_hl().wrapping_add(1)),
            0x33 => self.sp = self.sp.wrapping_add(1),

            0x0B => self.set_bc(self.get_bc().wrapping_sub(1)),
            0x1B => self.set_de(self.get_de().wrapping_sub(1)),
            0x2B => self.set_hl(self.get_hl().wrapping_sub(1)),
            0x3B => self.sp = self.sp.wrapping_sub(1),

            // 16-bit add: ADD HL, rr
            0x09 => {
                let value = self.get_bc();
                self.add_hl(value);
            }
            0x19 => {
                let value = self.get_de();
                self.add_hl(value);
            }
            0x29 => {
                let value = self.get_hl();
                self.add_hl(value);
            }
            0x39 => {
                let value = self.sp;
                self.add_hl(value);
            }

            // 8-bit arithmetic and logic instructions against register A
            // Covers the entire 0x80-0xBF block:
            // ADD/ADC/SUB/SBC/AND/XOR/OR/CP, each against B,C,D,E,H,L,(HL),A
            0x80..=0xBF => {
                let src = opcode & 0x07;
                let value = self.get_r8(src);

                match (opcode >> 3) & 0x07 {
                    0 => self.add(value),
                    1 => self.adc(value),
                    2 => self.sub(value),
                    3 => self.sbc(value),
                    4 => self.and(value),
                    5 => self.xor(value),
                    6 => self.or(value),
                    7 => self.cp(value),
                    _ => unreachable!(),
                }
            }

            // Immediate forms of the arithmetic/logic instructions above
            // ADD A, n
            0xC6 => {
                let value = self.fetch_byte();
                self.add(value);
            }

            // SUB n
            0xD6 => {
                let value = self.fetch_byte();
                self.sub(value);
            }

            // ADC A, n
            0xCE => {
                let value = self.fetch_byte();
                self.adc(value);
            }

            // SBC A, n
            0xDE => {
                let value = self.fetch_byte();
                self.sbc(value);
            }

            // AND n
            0xE6 => {
                let value = self.fetch_byte();
                self.and(value);
            }

            // XOR n
            0xEE => {
                let value = self.fetch_byte();
                self.xor(value);
            }

            // OR n
            0xF6 => {
                let value = self.fetch_byte();
                self.or(value);
            }

            // CP n
            0xFE => {
                let value = self.fetch_byte();
                self.cp(value);
            }

            // Misc single-byte flag/accumulator instructions
            // DAA - adjust A for BCD after an add/sub
            0x27 => self.daa(),

            // CPL - complement (flip all bits of) register A
            0x2F => {
                self.a = !self.a;
                self.set_subtract_flag(true);
                self.set_half_carry_flag(true);
            }

            // SCF - set carry flag
            0x37 => {
                self.set_carry_flag(true);
                self.set_subtract_flag(false);
                self.set_half_carry_flag(false);
            }

            // CCF - complement carry flag
            0x3F => {
                let carry = self.get_carry_flag();
                self.set_carry_flag(!carry);
                self.set_subtract_flag(false);
                self.set_half_carry_flag(false);
            }

            // Jump instructions
            // Change the Program Counter to alter program execution

            // JP nn
            // Jump to a 16-bit address
            0xC3 => {
                let address = self.fetch_word();
                self.jump(address);
            }

            // JP NZ, nn
            0xC2 => {
                let address = self.fetch_word();

                if !self.get_zero_flag() {
                    self.jump(address);
                    self.branch_extra_cycles = 4;
                }
            }

            // JP Z, nn
            0xCA => {
                let address = self.fetch_word();

                if self.get_zero_flag() {
                    self.jump(address);
                    self.branch_extra_cycles = 4;
                }
            }

            // JP NC, nn
            0xD2 => {
                let address = self.fetch_word();

                if !self.get_carry_flag() {
                    self.jump(address);
                    self.branch_extra_cycles = 4;
                }
            }

            // JP C, nn
            0xDA => {
                let address = self.fetch_word();

                if self.get_carry_flag() {
                    self.jump(address);
                    self.branch_extra_cycles = 4;
                }
            }

            // JP (HL)
            // Jump to the address held in HL (not the value at that address)
            0xE9 => {
                self.jump(self.get_hl());
            }

            // JR e
            // Relative jump using a signed offset
            0x18 => {
                let offset = self.fetch_signed_byte();
                self.jump_relative(offset);
            }

            // JR NZ, e
            0x20 => {
                let offset = self.fetch_signed_byte();

                if !self.get_zero_flag() {
                    self.jump_relative(offset);
                    self.branch_extra_cycles = 4;
                }
            }

            // JR Z, e
            0x28 => {
                let offset = self.fetch_signed_byte();

                if self.get_zero_flag() {
                    self.jump_relative(offset);
                    self.branch_extra_cycles = 4;
                }
            }

            // JR NC, e
            0x30 => {
                let offset = self.fetch_signed_byte();

                if !self.get_carry_flag() {
                    self.jump_relative(offset);
                    self.branch_extra_cycles = 4;
                }
            }

            // JR C, e
            0x38 => {
                let offset = self.fetch_signed_byte();

                if self.get_carry_flag() {
                    self.jump_relative(offset);
                    self.branch_extra_cycles = 4;
                }
            }

            // PUSH instructions
            // Store register pairs onto the stack

            // PUSH BC
            0xC5 => {
                self.push_word(self.get_bc());
            }

            // PUSH DE
            0xD5 => {
                self.push_word(self.get_de());
            }

            // PUSH HL
            0xE5 => {
                self.push_word(self.get_hl());
            }

            // PUSH AF
            0xF5 => {
                let af = ((self.a as u16) << 8) | self.f as u16;
                self.push_word(af);
            }

            // POP instructions
            // Restore register pairs from the stack

            // POP BC
            0xC1 => {
                let value = self.pop_word();
                self.set_bc(value);
            }

            // POP DE
            0xD1 => {
                let value = self.pop_word();
                self.set_de(value);
            }

            // POP HL
            0xE1 => {
                let value = self.pop_word();
                self.set_hl(value);
            }

            // POP AF
            0xF1 => {
                let value = self.pop_word();

                self.a = (value >> 8) as u8;

                // Lower 4 bits of F are always zero on the Game Boy CPU
                self.f = (value as u8) & 0xF0;
            }

            // CALL instructions
            // Save the return address and jump to a subroutine

            // CALL nn
            0xCD => {
                let address = self.fetch_word();
                self.call(address);
            }

            // CALL NZ, nn
            0xC4 => {
                let address = self.fetch_word();

                if !self.get_zero_flag() {
                    self.call(address);
                    self.branch_extra_cycles = 12;
                }
            }

            // CALL Z, nn
            0xCC => {
                let address = self.fetch_word();

                if self.get_zero_flag() {
                    self.call(address);
                    self.branch_extra_cycles = 12;
                }
            }

            // CALL NC, nn
            0xD4 => {
                let address = self.fetch_word();

                if !self.get_carry_flag() {
                    self.call(address);
                    self.branch_extra_cycles = 12;
                }
            }

            // CALL C, nn
            0xDC => {
                let address = self.fetch_word();

                if self.get_carry_flag() {
                    self.call(address);
                    self.branch_extra_cycles = 12;
                }
            }

            // RET instructions
            // Return from a subroutine

            // RET
            0xC9 => {
                self.ret();
            }

            // RET NZ
            0xC0 => {
                if !self.get_zero_flag() {
                    self.ret();
                    self.branch_extra_cycles = 12;
                }
            }

            // RET Z
            0xC8 => {
                if self.get_zero_flag() {
                    self.ret();
                    self.branch_extra_cycles = 12;
                }
            }

            // RET NC
            0xD0 => {
                if !self.get_carry_flag() {
                    self.ret();
                    self.branch_extra_cycles = 12;
                }
            }

            // RET C
            0xD8 => {
                if self.get_carry_flag() {
                    self.ret();
                    self.branch_extra_cycles = 12;
                }
            }

            // RETI
            // Returns from a subroutine and immediately re-enables interrupts
            // (unlike EI, there's no one-instruction delay for RETI)
            0xD9 => {
                self.ret();
                self.ime = true;
                self.ime_delay = 0;
            }

            // DI - disable interrupts immediately
            0xF3 => {
                self.ime = false;
                self.ime_delay = 0;
            }

            // EI - enable interrupts after the next instruction completes
            0xFB => {
                self.ime_delay = 2;
            }

            // Restart (RST) instructions
            // Jump to one of the fixed interrupt vectors

            // RST 00H
            0xC7 => {
                self.rst(0x0000);
            }

            // RST 08H
            0xCF => {
                self.rst(0x0008);
            }

            // RST 10H
            0xD7 => {
                self.rst(0x0010);
            }

            // RST 18H
            0xDF => {
                self.rst(0x0018);
            }

            // RST 20H
            0xE7 => {
                self.rst(0x0020);
            }

            // RST 28H
            0xEF => {
                self.rst(0x0028);
            }

            // RST 30H
            0xF7 => {
                self.rst(0x0030);
            }

            // RST 38H
            0xFF => {
                self.rst(0x0038);
            }

            // CB-prefixed opcodes are handled by the early return at the
            // top of this function, before this match even runs.

            _ => {
                println!("Unknown opcode: {:02X}", opcode);
            }
        }

        CYCLES[opcode as usize] + self.branch_extra_cycles
    }

    // Executes the entire CB-prefixed table (256 opcodes) generically.
    // Layout: bits [7:6] select the group, bits [5:3] select bit index
    // (for BIT/RES/SET) or operation (for rotate/shift), bits [2:0] select
    // the register/memory operand via get_r8/set_r8. Returns the T-cycle
    // cost: register operands always cost 8; (HL) costs 16, except BIT
    // (HL) which is 12 since it doesn't write the result back.
    fn execute_cb(&mut self, opcode: u8) -> u8 {
        let reg_idx = opcode & 0x07;
        let bit_idx = (opcode >> 3) & 0x07;
        let is_hl = reg_idx == 6;

        match opcode {
            // 0x00-0x3F: rotates and shifts
            0x00..=0x3F => {
                let value = self.get_r8(reg_idx);

                let result = match (opcode >> 3) & 0x07 {
                    0 => self.rlc(value),
                    1 => self.rrc(value),
                    2 => self.rl(value),
                    3 => self.rr(value),
                    4 => self.sla(value),
                    5 => self.sra(value),
                    6 => self.swap(value),
                    7 => self.srl(value),
                    _ => unreachable!(),
                };

                self.set_r8(reg_idx, result);
                if is_hl { 16 } else { 8 }
            }

            // 0x40-0x7F: BIT b, r - test a bit, doesn't modify the operand
            0x40..=0x7F => {
                let value = self.get_r8(reg_idx);
                self.bit(bit_idx, value);
                if is_hl { 12 } else { 8 }
            }

            // 0x80-0xBF: RES b, r - clear a bit
            0x80..=0xBF => {
                let value = self.get_r8(reg_idx);
                self.set_r8(reg_idx, value & !(1 << bit_idx));
                if is_hl { 16 } else { 8 }
            }

            // 0xC0-0xFF: SET b, r - set a bit
            0xC0..=0xFF => {
                let value = self.get_r8(reg_idx);
                self.set_r8(reg_idx, value | (1 << bit_idx));
                if is_hl { 16 } else { 8 }
            }
        }
    }

    // Helper functions for 16-bit register pairs
    // Game Boy combines two 8-bit registers into one 16-bit register:
    // BC, DE and HL

    // Returns the combined 16-bit BC register
    fn get_bc(&self) -> u16 {
        ((self.b as u16) << 8) | self.c as u16
    }

    // Splits a 16-bit value into registers B and C
    fn set_bc(&mut self, value: u16) {
        self.b = (value >> 8) as u8;
        self.c = value as u8;
    }

    // Returns the combined 16-bit DE register
    fn get_de(&self) -> u16 {
        ((self.d as u16) << 8) | self.e as u16
    }

    // Splits a 16-bit value into registers D and E
    fn set_de(&mut self, value: u16) {
        self.d = (value >> 8) as u8;
        self.e = value as u8;
    }

    // Returns the combined 16-bit HL register
    fn get_hl(&self) -> u16 {
        ((self.h as u16) << 8) | self.l as u16
    }

    // Splits a 16-bit value into registers H and L
    fn set_hl(&mut self, value: u16) {
        self.h = (value >> 8) as u8;
        self.l = value as u8;
    }

    // Returns the current value of the Stack Pointer
    fn get_sp(&self) -> u16 {
        self.sp
    }

    // Sets or clears the Zero flag
    fn set_zero_flag(&mut self, value: bool) {
        if value {
            self.f |= 0x80;
        } else {
            self.f &= !0x80;
        }
    }

    // Sets or clears the Subtract flag
    fn set_subtract_flag(&mut self, value: bool) {
        if value {
            self.f |= 0x40;
        } else {
            self.f &= !0x40;
        }
    }

    // Sets or clears the Half-Carry flag
    fn set_half_carry_flag(&mut self, value: bool) {
        if value {
            self.f |= 0x20;
        } else {
            self.f &= !0x20;
        }
    }

    // Sets or clears the Carry flag
    fn set_carry_flag(&mut self, value: bool) {
        if value {
            self.f |= 0x10;
        } else {
            self.f &= !0x10;
        }
    }

    // Returns true if the Carry flag is currently set
    fn get_carry_flag(&self) -> bool {
        (self.f & 0x10) != 0
    }

    // Returns true if the Zero flag is currently set
    fn get_zero_flag(&self) -> bool {
        (self.f & 0x80) != 0
    }

    // Returns true if the Subtract flag is currently set
    fn get_subtract_flag(&self) -> bool {
        (self.f & 0x40) != 0
    }

    // Returns true if the Half-Carry flag is currently set
    fn get_half_carry_flag(&self) -> bool {
        (self.f & 0x20) != 0
    }

    // Increments an 8-bit value and updates CPU flags
    fn inc(&mut self, value: u8) -> u8 {

        // Increment using wrapping arithmetic to avoid overflow
        let result = value.wrapping_add(1);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);

        self.set_half_carry_flag((value & 0x0F) + 1 > 0x0F);

        result
    }

    // Decrements an 8-bit value and updates CPU flags
    fn dec(&mut self, value: u8) -> u8 {

        // Decrement using wrapping arithmetic to avoid underflow
        let result = value.wrapping_sub(1);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(true);

        self.set_half_carry_flag((value & 0x0F) == 0);

        result
    }

    // Adds two 8-bit values and updates CPU flags
    fn add(&mut self, value: u8) {

        let a = self.a;

        let result = a.wrapping_add(value);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);

        self.set_half_carry_flag((a & 0x0F) + (value & 0x0F) > 0x0F);

        self.set_carry_flag((a as u16 + value as u16) > 0xFF);

        self.a = result;
    }

    // Adds a 16-bit value into HL and updates CPU flags
    // (Zero flag is untouched by this instruction on real hardware)
    fn add_hl(&mut self, value: u16) {

        let hl = self.get_hl();

        let result = hl.wrapping_add(value);

        self.set_subtract_flag(false);

        self.set_half_carry_flag((hl & 0x0FFF) + (value & 0x0FFF) > 0x0FFF);

        self.set_carry_flag((hl as u32) + (value as u32) > 0xFFFF);

        self.set_hl(result);
    }

    // Subtracts an 8-bit value from register A and updates CPU flags
    fn sub(&mut self, value: u8) {

        let a = self.a;

        let result = a.wrapping_sub(value);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(true);

        self.set_half_carry_flag((a & 0x0F) < (value & 0x0F));

        self.set_carry_flag(a < value);

        self.a = result;
    }

    // Adds an 8-bit value and the Carry flag to register A
    fn adc(&mut self, value: u8) {

        let carry = if self.get_carry_flag() { 1 } else { 0 };

        let a = self.a;

        let result = a.wrapping_add(value).wrapping_add(carry);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);

        self.set_half_carry_flag(
            (a & 0x0F) + (value & 0x0F) + carry > 0x0F
        );

        self.set_carry_flag(
            (a as u16 + value as u16 + carry as u16) > 0xFF
        );

        self.a = result;
    }

    // Subtracts an 8-bit value and the Carry flag from register A
    fn sbc(&mut self, value: u8) {

        let carry = if self.get_carry_flag() { 1 } else { 0 };

        let a = self.a;

        let result = a.wrapping_sub(value).wrapping_sub(carry);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(true);

        self.set_half_carry_flag(
            (a & 0x0F) < ((value & 0x0F) + carry)
        );

        self.set_carry_flag(
            (a as u16) < (value as u16 + carry as u16)
        );

        self.a = result;
    }

    // Adjusts register A into valid BCD form after an ADD/SUB/ADC/SBC
    fn daa(&mut self) {

        let mut a = self.a;
        let mut adjust = 0u8;
        let mut carry = false;

        if self.get_half_carry_flag() || (!self.get_subtract_flag() && (a & 0x0F) > 9) {
            adjust |= 0x06;
        }

        if self.get_carry_flag() || (!self.get_subtract_flag() && a > 0x99) {
            adjust |= 0x60;
            carry = true;
        }

        a = if self.get_subtract_flag() {
            a.wrapping_sub(adjust)
        } else {
            a.wrapping_add(adjust)
        };

        self.set_zero_flag(a == 0);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        self.a = a;
    }

    // Reads the next signed byte from memory
    fn fetch_signed_byte(&mut self) -> i8 {
        self.fetch_byte() as i8
    }

    // Shared by ADD SP,e and LD HL,SP+e. Real hardware computes the
    // Half-Carry/Carry flags from an 8-bit unsigned addition of SP's low
    // byte and the offset's raw bit pattern - not from the signed 16-bit
    // result - so a "negative" offset can still set Carry. Zero/Subtract
    // are always cleared by both instructions.
    fn sp_plus_signed(&mut self, offset: i8) -> u16 {

        let sp = self.sp;
        let result = (sp as i32 + offset as i32) as u16;

        let sp_low = sp as u8;
        let offset_bits = offset as u8;

        self.set_zero_flag(false);
        self.set_subtract_flag(false);
        self.set_half_carry_flag((sp_low & 0x0F) + (offset_bits & 0x0F) > 0x0F);
        self.set_carry_flag((sp_low as u16) + (offset_bits as u16) > 0xFF);

        result
    }

    // Updates the Program Counter to the specified address
    fn jump(&mut self, address: u16) {
        self.pc = address;
    }

    // Performs a relative jump
    fn jump_relative(&mut self, offset: i8) {
        self.pc = ((self.pc as i32) + offset as i32) as u16;
    }

    // Performs bitwise AND with register A
    fn and(&mut self, value: u8) {

        self.a &= value;

        self.set_zero_flag(self.a == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(true);
        self.set_carry_flag(false);
    }

    // Performs bitwise OR with register A
    fn or(&mut self, value: u8) {

        self.a |= value;

        self.set_zero_flag(self.a == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(false);
    }

    // Performs bitwise XOR with register A
    fn xor(&mut self, value: u8) {

        self.a ^= value;

        self.set_zero_flag(self.a == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(false);
    }

    // Compares an 8-bit value with register A without changing A
    fn cp(&mut self, value: u8) {

        let a = self.a;

        let result = a.wrapping_sub(value);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(true);

        self.set_half_carry_flag(
            (a & 0x0F) < (value & 0x0F)
        );

        self.set_carry_flag(a < value);
    }

    // Tests a single bit of a value and updates flags (Zero, Subtract, Half-Carry)
    // Carry flag is untouched by BIT on real hardware
    fn bit(&mut self, bit: u8, value: u8) {
        self.set_zero_flag((value & (1 << bit)) == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(true);
    }

    // Pushes one byte onto the stack
    fn push_byte(&mut self, value: u8) {
        self.sp = self.sp.wrapping_sub(1);
        self.bus.write_byte(self.sp, value);
    }

    // Pops one byte from the stack
    fn pop_byte(&mut self) -> u8 {
        let value = self.bus.read_byte(self.sp);
        self.sp = self.sp.wrapping_add(1);
        value
    }

    // Pushes a 16-bit value onto the stack
    fn push_word(&mut self, value: u16) {

        let high = (value >> 8) as u8;
        let low = value as u8;

        self.push_byte(high);
        self.push_byte(low);
    }

    // Pops a 16-bit value from the stack
    fn pop_word(&mut self) -> u16 {

        let low = self.pop_byte() as u16;
        let high = self.pop_byte() as u16;

        (high << 8) | low
    }

    // Calls a subroutine by saving the return address on the stack
    fn call(&mut self, address: u16) {

        // Save the address of the next instruction
        self.push_word(self.pc);

        // Jump to the subroutine
        self.pc = address;
    }

    // Returns from a subroutine
    fn ret(&mut self) {

        // Restore the previously saved program counter
        self.pc = self.pop_word();
    }

    // Performs a restart by saving the current Program Counter and jumping to one of the fixed restart vectors
    fn rst(&mut self, address: u16) {

        // Save the return address
        self.push_word(self.pc);

        // Jump to the restart vector
        self.pc = address;
    }

    // Rotates an 8-bit value left
    // Bit 7 becomes both the Carry flag and bit 0
    fn rlc(&mut self, value: u8) -> u8 {
        let carry = (value & 0x80) != 0;
        let result = (value << 1) | (carry as u8);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        result
    }

    // Rotates an 8-bit value right
    // Bit 0 becomes both the Carry flag and bit 7
    fn rrc(&mut self, value: u8) -> u8 {
        let carry = (value & 0x01) != 0;
        let result = (value >> 1) | ((carry as u8) << 7);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        result
    }

    // Rotates an 8-bit value left through the Carry flag
    fn rl(&mut self, value: u8) -> u8 {
        let carry_in = if self.get_carry_flag() { 1 } else { 0 };
        let carry_out = (value & 0x80) != 0;

        let result = (value << 1) | carry_in;

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry_out);

        result
    }

    // Rotates an 8-bit value right through the Carry flag
    fn rr(&mut self, value: u8) -> u8 {
        let carry_in = if self.get_carry_flag() { 0x80 } else { 0 };
        let carry_out = (value & 1) != 0;

        let result = (value >> 1) | carry_in;

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry_out);

        result
    }

    // Performs an arithmetic left shift
    // Bit 7 moves into the Carry flag
    fn sla(&mut self, value: u8) -> u8 {
        let carry = (value & 0x80) != 0;
        let result = value << 1;

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        result
    }

    // Performs an arithmetic right shift
    // The most significant bit is preserved
    fn sra(&mut self, value: u8) -> u8 {
        let carry = (value & 1) != 0;
        let result = (value >> 1) | (value & 0x80);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        result
    }

    // Performs a logical right shift
    // Bit 7 is filled with zero
    fn srl(&mut self, value: u8) -> u8 {
        let carry = (value & 1) != 0;
        let result = value >> 1;

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(carry);

        result
    }

    // Swaps the upper and lower nibbles of an 8-bit value
    fn swap(&mut self, value: u8) -> u8 {
        let result = (value << 4) | (value >> 4);

        self.set_zero_flag(result == 0);
        self.set_subtract_flag(false);
        self.set_half_carry_flag(false);
        self.set_carry_flag(false);

        result
    }
}