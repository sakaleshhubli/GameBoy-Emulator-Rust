mod bus;
mod cartridge;
mod cpu;
mod ppu;
mod joypad;
mod sound;

use std::collections::VecDeque;
use std::env;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bus::Bus;
use cartridge::Cartridge;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpu::Cpu;
use joypad::Button;
use minifb::{Key, Window, WindowOptions};
use ppu::{SCREEN_HEIGHT, SCREEN_WIDTH};

// T-cycles per frame at the DMG's 4.194304 MHz clock and its real
// (slightly-off-60Hz) 59.7275 Hz refresh rate.
const CYCLES_PER_FRAME: u32 = 70224;

// Classic four-shade DMG "green" palette as 0xRRGGBB, one entry per 2-bit
// shade index (0 = lightest, 3 = darkest). Swap this for any other palette.
const PALETTE: [u32; 4] = [0x9BBC0F, 0x8BAC0F, 0x306230, 0x0F380F];

// Upper bound on how many buffered audio samples we let build up if the
// output callback falls behind (e.g. right after the stream opens) - caps
// perceived latency instead of letting it grow unbounded.
const MAX_BUFFERED_SAMPLES: usize = 8192;

// Opens the default output device and starts a stream that just drains
// `shared_buffer` (interleaved, matching the device's own channel count).
// Returns the live Stream - it stops producing sound the moment this is
// dropped, so main() has to hang onto it for as long as it wants audio.
fn start_audio_stream(
    shared_buffer: Arc<Mutex<VecDeque<f32>>>,
) -> Result<(cpal::Stream, u32), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no audio output device available")?;

    let supported_config = device.default_output_config()?;
    let sample_rate = supported_config.sample_rate().0;
    let channels = supported_config.channels() as usize;
    let stream_config: cpal::StreamConfig = supported_config.into();

    let stream = device.build_output_stream(
        &stream_config,
        move |data: &mut [f32], _| {
            let mut buffer = shared_buffer.lock().unwrap();

            for frame in data.chunks_mut(channels) {
                let left = buffer.pop_front().unwrap_or(0.0);
                let right = buffer.pop_front().unwrap_or(left);

                match frame.len() {
                    1 => frame[0] = (left + right) * 0.5,
                    _ => {
                        frame[0] = left;
                        frame[1] = right;
                        for sample in frame.iter_mut().skip(2) {
                            *sample = 0.0;
                        }
                    }
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )?;

    stream.play()?;

    Ok((stream, sample_rate))
}

fn main() {

    // Collect all command-line arguments
    let args: Vec<String> = env::args().collect();

    // The first argument should be the Game Boy ROM file
    if args.len() < 2 {
        println!("Usage: cargo run -- <rom.gb>");
        return;
    }

    let audio_buffer: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));

    // Audio is a nice-to-have, not a reason to refuse to run the emulator
    // at all - if no device is available, keep going silently.
    let (_audio_stream, sample_rate) = match start_audio_stream(Arc::clone(&audio_buffer)) {
        Ok((stream, rate)) => (Some(stream), rate),
        Err(err) => {
            eprintln!("audio disabled: {err}");
            (None, 44_100)
        }
    };

    // Load the ROM from disk into a Cartridge object
    let cartridge = Cartridge::load(&args[1]).unwrap();

    // Create the memory bus and connect it to the cartridge
    let bus = Bus::new(cartridge, sample_rate);

    // Create the CPU and give it access to the memory bus
    let mut cpu = Cpu::new(bus);

    let mut window = Window::new(
        "gbrs",
        SCREEN_WIDTH,
        SCREEN_HEIGHT,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )
    .expect("failed to open display window");

    // minifb wants one u32 (0xRRGGBB) per pixel, row-major - same layout
    // the PPU's framebuffer already uses, just needing shade -> color.
    let mut window_buffer = vec![0u32; SCREEN_WIDTH * SCREEN_HEIGHT];

    let frame_duration = Duration::from_nanos(16_742_706); // 1 / 59.7275 Hz

    while window.is_open() && !window.is_key_down(Key::Escape) {
        let frame_start = Instant::now();

        if window.is_key_down(Key::Right) {
            cpu.bus_mut().press_button(Button::Right);
        } else {
            cpu.bus_mut().release_button(Button::Right);
        }

        if window.is_key_down(Key::Left) {
            cpu.bus_mut().press_button(Button::Left);
        } else {
            cpu.bus_mut().release_button(Button::Left);
        }

        if window.is_key_down(Key::Up) {
            cpu.bus_mut().press_button(Button::Up);
        } else {
            cpu.bus_mut().release_button(Button::Up);
        }

        if window.is_key_down(Key::Down) {
            cpu.bus_mut().press_button(Button::Down);
        } else {
            cpu.bus_mut().release_button(Button::Down);
        }

        if window.is_key_down(Key::Z) {
            cpu.bus_mut().press_button(Button::A);
        } else {
            cpu.bus_mut().release_button(Button::A);
        }

        if window.is_key_down(Key::X) {
            cpu.bus_mut().press_button(Button::B);
        } else {
            cpu.bus_mut().release_button(Button::B);
        }

        if window.is_key_down(Key::Enter) {
            cpu.bus_mut().press_button(Button::Start);
        } else {
            cpu.bus_mut().release_button(Button::Start);
        }

        if window.is_key_down(Key::RightShift) {
            cpu.bus_mut().press_button(Button::Select);
        } else {
            cpu.bus_mut().release_button(Button::Select);
        }

        // Run the CPU until it's advanced roughly one full frame's worth
        // of T-cycles. This can overshoot slightly since instructions
        // aren't split mid-execution, which is fine for pacing purposes.
        let mut cycles_this_frame = 0u32;
        while cycles_this_frame < CYCLES_PER_FRAME {
            cycles_this_frame += cpu.step();
        }

        let framebuffer = cpu.bus().framebuffer();
        for (pixel, &shade) in window_buffer.iter_mut().zip(framebuffer.iter()) {
            *pixel = PALETTE[shade as usize];
        }

        window
            .update_with_buffer(&window_buffer, SCREEN_WIDTH, SCREEN_HEIGHT)
            .expect("failed to present frame");

        // Hand this frame's worth of audio samples to the output stream,
        // capping how far the buffer's allowed to grow so a slow start
        // (or a callback hiccup) doesn't turn into ever-growing latency.
        let samples = cpu.bus_mut().take_audio_samples();
        {
            let mut buffer = audio_buffer.lock().unwrap();
            buffer.extend(samples);

            let excess = buffer.len().saturating_sub(MAX_BUFFERED_SAMPLES);
            for _ in 0..excess {
                buffer.pop_front();
            }
        }

        // Cap to roughly the real Game Boy's refresh rate instead of
        // running as fast as the host CPU allows.
        let elapsed = frame_start.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration - elapsed);
        }
    }
}