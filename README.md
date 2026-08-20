# GameBoy-Emulator-Rust

A Game Boy (DMG) emulator written in Rust. The project implements the Sharp LR35902 CPU, memory bus, PPU, APU, cartridge/MBC banking, and joypad handling required to run homebrew Game Boy ROMs.

## Overview

This emulator targets the original Game Boy (DMG) hardware, not the Game Boy Advance. It is built as a set of independent modules connected through a central memory bus, following the general architecture of the real hardware:

- **CPU** - Sharp LR35902 instruction set, including the CB-prefixed opcode table, interrupt handling, and per-instruction T-cycle timing.
- **Bus** - Central memory map connecting the CPU to ROM, RAM, PPU, APU, timer, joypad, and interrupt registers.
- **PPU** - Scanline-based graphics renderer producing background, window, and sprite output with correct mode timing (OAM scan, drawing, HBlank, VBlank) for LY/STAT-driven behavior.
- **APU (Sound)** - All four DMG audio channels: two square wave channels (one with frequency sweep), a wave channel, and a noise channel, mixed and output as PCM samples at the host audio device's sample rate.
- **Cartridge** - ROM loading and memory bank controller emulation, supporting ROM-only cartridges, MBC1, and MBC3 (including MBC3 RTC registers).
- **Joypad** - Standard eight-button Game Boy controller mapped to register 0xFF00, with edge-triggered interrupt requests.
- **Timer** - DIV/TIMA/TMA/TAC registers with configurable increment frequency and overflow interrupt handling.

## Scope and Limitations

This implementation prioritizes correctness for standard gameplay over full cycle accuracy. Known simplifications include:

- The PPU renders each scanline in a single pass once drawing completes, rather than through a per-pixel FIFO. Backgrounds, windows, and sprites render correctly, but effects relying on mid-scanline register writes are not reproduced.
- OAM DMA transfers complete instantly rather than over the real 160 M-cycle window, so CPU access restrictions during DMA are not enforced.
- The timer does not reproduce the DIV-write glitch exhibited by some hardware test ROMs.
- Cartridge types other than ROM-only, MBC1, and MBC3 are detected and loaded, but bank switching for them is not implemented; only the fixed first ROM bank is accessible.
- MBC3 real-time clock registers hold whatever value the game last wrote and do not advance against wall-clock time.
- The APU's frequency sweep performs a single overflow check per step rather than the double-check performed by real hardware, and does not reproduce the NRx2 "zombie mode" envelope quirk.

These simplifications are documented inline in the corresponding source files and do not affect standard gameplay in the majority of homebrew titles.

## Requirements

- Rust (stable toolchain) and Cargo
- A working audio output device (optional - the emulator runs without one, with audio disabled)
- A Game Boy ROM file in `.gb` format

## Building

```
cargo build --release
```

## Running

```
cargo run --release -- <path-to-rom.gb>
```

The emulator opens a window at the native Game Boy resolution (160x144) and renders at approximately 59.7275 Hz, matching the DMG's real refresh rate.

## Controls

| Game Boy Button | Keyboard Key |
|------------------|--------------|
| D-Pad            | Arrow keys   |
| A                | Z            |
| B                | X            |
| Start            | Enter        |
| Select           | Right Shift  |
| Quit             | Escape       |

## Project Structure

```
src/
  main.rs       Application entry point, window/audio setup, main loop
  cpu.rs        Sharp LR35902 CPU core and instruction execution
  bus.rs        Memory bus, timer, and interrupt dispatch
  ppu.rs        Picture Processing Unit and framebuffer generation
  sound.rs      Audio Processing Unit (all four channels)
  cartridge.rs  ROM loading and memory bank controller emulation
  joypad.rs     Controller input handling
```

## Testing

The emulator supports Blargg-style test ROMs through the serial output port (0xFF01/0xFF02). When a test ROM writes diagnostic text over serial, it is printed directly to standard output, allowing PASS/FAIL results to be read from the console without requiring a working display.

## License

No license has been specified for this project.
