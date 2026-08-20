// The PPU (Picture Processing Unit) turns VRAM/OAM contents into pixels.
//
// This is a scanline renderer, not a cycle-accurate pixel FIFO: it tracks
// mode timing (OAM scan / drawing / hblank / vblank) T-cycle by T-cycle so
// LY/STAT/interrupts behave correctly, but it draws an entire scanline in
// one shot the moment drawing (mode 3) ends. That's enough to correctly
// display backgrounds, windows and sprites for the vast majority of
// homebrew, but it won't reproduce effects that rely on mid-scanline
// register writes (raster splits, mid-line palette swaps, etc).

pub const SCREEN_WIDTH: usize = 160;
pub const SCREEN_HEIGHT: usize = 144;

const OAM_SCAN_CYCLES: u32 = 80;
const DRAWING_CYCLES: u32 = 172;
const SCANLINE_CYCLES: u32 = 456;
const TOTAL_LINES: u8 = 154;

#[derive(PartialEq, Clone, Copy)]
enum PpuMode {
    HBlank,  // mode 0
    VBlank,  // mode 1
    OamScan, // mode 2
    Drawing, // mode 3
}

// Interrupts requested by the PPU during a tick(). The Bus owns
// interrupt_flag, so tick() hands back what fired instead of poking it
// directly - same pattern as Bus::request_interrupt for the timer.
pub struct PpuInterrupts {
    pub vblank: bool,
    pub stat: bool,
}

pub struct Ppu {
    // Video RAM (8 KB) - tile data and background/window tile maps
    vram: [u8; 0x2000],

    // Object Attribute Memory (160 bytes) - sprite positions/attributes
    oam: [u8; 0xA0],

    // LCDC (0xFF40) - LCD/PPU control
    lcdc: u8,

    // STAT (0xFF41) - only the writable interrupt-enable bits (3-6) are
    // stored here; the mode and coincidence bits are read-only and are
    // computed on the fly from `mode`/`ly`/`lyc` in read_register.
    stat: u8,

    scy: u8,
    scx: u8,
    ly: u8,
    lyc: u8,
    bgp: u8,
    obp0: u8,
    obp1: u8,
    wy: u8,
    wx: u8,

    mode: PpuMode,

    // Position within the current 456 T-cycle scanline
    dot: u32,

    // The window has its own internal line counter, separate from LY: it
    // only advances on scanlines where the window was actually drawn, so
    // toggling window-enable mid-frame doesn't desync its tile row.
    window_line: u8,

    // Previous state of the OR of all enabled STAT interrupt sources.
    // The STAT interrupt is edge-triggered off this line going 0->1, not
    // level-triggered - without this a game would get a fresh interrupt
    // every single T-cycle a condition stayed true.
    stat_irq_line: bool,

    // One shade index (0-3, already run through the relevant palette) per
    // pixel, row-major. 0 is the lightest shade, 3 the darkest.
    framebuffer: [u8; SCREEN_WIDTH * SCREEN_HEIGHT],
}

impl Ppu {
    pub fn new() -> Self {
        Self {
            vram: [0; 0x2000],
            oam: [0; 0xA0],

            // Values the real boot ROM leaves behind when it hands off to
            // the game, since we don't emulate the boot ROM itself.
            lcdc: 0x91,
            stat: 0,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xFC,
            obp0: 0xFF,
            obp1: 0xFF,
            wy: 0,
            wx: 0,

            mode: PpuMode::OamScan,
            dot: 0,
            window_line: 0,
            stat_irq_line: false,

            framebuffer: [0; SCREEN_WIDTH * SCREEN_HEIGHT],
        }
    }

    // Advances the PPU by the given number of T-cycles. Called once per
    // CPU step with that instruction's cycle cost, same as Bus::tick for
    // the timer. Returns which interrupts fired anywhere in that span.
    pub fn tick(&mut self, cycles: u32) -> PpuInterrupts {
        let mut result = PpuInterrupts {
            vblank: false,
            stat: false,
        };

        for _ in 0..cycles {
            let (vblank, stat) = self.tick_one();
            result.vblank |= vblank;
            result.stat |= stat;
        }

        result
    }

    // Advances mode/LY state by a single T-cycle and returns
    // (vblank_irq_fired, stat_irq_fired) for that cycle.
    fn tick_one(&mut self) -> (bool, bool) {
        // LCD off: PPU is idle, LY and mode sit at their blanked values
        // and nothing here generates interrupts.
        if self.lcdc & 0x80 == 0 {
            self.dot = 0;
            self.ly = 0;
            self.mode = PpuMode::HBlank;
            self.stat_irq_line = false;
            return (false, false);
        }

        self.dot += 1;
        let mut vblank_irq = false;

        match self.mode {
            PpuMode::OamScan if self.dot == OAM_SCAN_CYCLES => {
                self.mode = PpuMode::Drawing;
            }

            PpuMode::Drawing if self.dot == OAM_SCAN_CYCLES + DRAWING_CYCLES => {
                self.render_scanline();
                self.mode = PpuMode::HBlank;
            }

            PpuMode::HBlank if self.dot == SCANLINE_CYCLES => {
                self.dot = 0;
                self.ly += 1;

                if self.ly == SCREEN_HEIGHT as u8 {
                    self.mode = PpuMode::VBlank;
                    self.window_line = 0;
                    vblank_irq = true;
                } else {
                    self.mode = PpuMode::OamScan;
                }
            }

            PpuMode::VBlank if self.dot == SCANLINE_CYCLES => {
                self.dot = 0;
                self.ly += 1;

                if self.ly == TOTAL_LINES {
                    self.ly = 0;
                    self.mode = PpuMode::OamScan;
                }
            }

            _ => {}
        }

        let stat_irq = self.update_stat_irq();

        (vblank_irq, stat_irq)
    }

    // Re-evaluates the OR of all STAT interrupt sources currently enabled
    // and fires only on the rising edge (see stat_irq_line doc comment).
    fn update_stat_irq(&mut self) -> bool {
        let lyc_match = self.ly == self.lyc;

        let source_active = (lyc_match && self.stat & 0x40 != 0)
            || (self.mode == PpuMode::OamScan && self.stat & 0x20 != 0)
            || (self.mode == PpuMode::VBlank && self.stat & 0x10 != 0)
            || (self.mode == PpuMode::HBlank && self.stat & 0x08 != 0);

        let fired = source_active && !self.stat_irq_line;
        self.stat_irq_line = source_active;

        fired
    }

    // === CPU-facing memory access ===
    //
    // VRAM/OAM are only open to the CPU outside the modes where the PPU
    // itself is reading them (mode 3 for VRAM, modes 2-3 for OAM). Real
    // hardware enforces this too; skipping it would let a game "see" PPU
    // internals it never could on real hardware, so writes are dropped
    // and reads return the same open-bus 0xFF used elsewhere on the bus.

    pub fn read_vram(&self, address: u16) -> u8 {
        if self.mode == PpuMode::Drawing {
            return 0xFF;
        }
        self.vram[(address - 0x8000) as usize]
    }

    pub fn write_vram(&mut self, address: u16, value: u8) {
        if self.mode == PpuMode::Drawing {
            return;
        }
        self.vram[(address - 0x8000) as usize] = value;
    }

    pub fn read_oam(&self, address: u16) -> u8 {
        if matches!(self.mode, PpuMode::OamScan | PpuMode::Drawing) {
            return 0xFF;
        }
        self.oam[(address - 0xFE00) as usize]
    }

    pub fn write_oam(&mut self, address: u16, value: u8) {
        if matches!(self.mode, PpuMode::OamScan | PpuMode::Drawing) {
            return;
        }
        self.oam[(address - 0xFE00) as usize] = value;
    }

    // Direct OAM write used by Bus's OAM DMA handler - bypasses the normal
    // mode gating, matching real DMA which isn't blocked by PPU mode.
    pub fn dma_write_oam(&mut self, offset: u8, value: u8) {
        self.oam[offset as usize] = value;
    }

    pub fn read_register(&self, address: u16) -> u8 {
        match address {
            0xFF40 => self.lcdc,
            0xFF41 => 0x80 | self.stat | self.coincidence_bit() | self.mode_bits(),
            0xFF42 => self.scy,
            0xFF43 => self.scx,
            0xFF44 => self.ly,
            0xFF45 => self.lyc,
            0xFF47 => self.bgp,
            0xFF48 => self.obp0,
            0xFF49 => self.obp1,
            0xFF4A => self.wy,
            0xFF4B => self.wx,
            _ => 0xFF,
        }
    }

    pub fn write_register(&mut self, address: u16, value: u8) {
        match address {
            0xFF40 => self.lcdc = value,
            0xFF41 => self.stat = value & 0x78,
            0xFF42 => self.scy = value,
            0xFF43 => self.scx = value,
            0xFF44 => {} // LY is read-only - writes are ignored
            0xFF45 => self.lyc = value,
            0xFF47 => self.bgp = value,
            0xFF48 => self.obp0 = value,
            0xFF49 => self.obp1 = value,
            0xFF4A => self.wy = value,
            0xFF4B => self.wx = value,
            _ => {}
        }
    }

    fn mode_bits(&self) -> u8 {
        match self.mode {
            PpuMode::HBlank => 0,
            PpuMode::VBlank => 1,
            PpuMode::OamScan => 2,
            PpuMode::Drawing => 3,
        }
    }

    fn coincidence_bit(&self) -> u8 {
        if self.ly == self.lyc {
            0x04
        } else {
            0
        }
    }

    // The finished frame, one shade index (0-3) per pixel, row-major.
    // Multiply up to RGB / index into a chosen 4-color palette at the
    // display layer - the PPU itself doesn't know about actual colors.
    pub fn framebuffer(&self) -> &[u8; SCREEN_WIDTH * SCREEN_HEIGHT] {
        &self.framebuffer
    }

    // === Rendering ===

    // Renders the current line (self.ly) into the framebuffer. Called once,
    // right as mode 3 (Drawing) ends - see tick_one.
    fn render_scanline(&mut self) {
        let line = self.ly as usize;

        // Raw (pre-palette) BG/window color index per pixel this line,
        // needed afterward to resolve sprite-behind-background priority.
        let mut bg_index = [0u8; SCREEN_WIDTH];

        if self.lcdc & 0x01 != 0 {
            self.render_background(line, &mut bg_index);

            let window_visible =
                self.lcdc & 0x20 != 0 && self.ly >= self.wy && self.wx <= 166;

            if window_visible {
                self.render_window(line, &mut bg_index);
            }
        } else {
            // BG/window disabled: the whole line is the lightest shade
            for x in 0..SCREEN_WIDTH {
                self.set_pixel(x, line, 0);
            }
        }

        if self.lcdc & 0x02 != 0 {
            self.render_sprites(line, &bg_index);
        }
    }

    fn render_background(&mut self, line: usize, bg_index: &mut [u8; SCREEN_WIDTH]) {
        let tile_map_base: u16 = if self.lcdc & 0x08 != 0 { 0x9C00 } else { 0x9800 };
        let signed_tiles = self.lcdc & 0x10 == 0;

        let y = self.scy.wrapping_add(line as u8);
        let tile_row = (y / 8) as u16;
        let pixel_row = (y % 8) as u16;

        for x in 0..SCREEN_WIDTH {
            let scrolled_x = self.scx.wrapping_add(x as u8);
            let tile_col = (scrolled_x / 8) as u16;
            let pixel_col = scrolled_x % 8;

            let map_addr = tile_map_base + tile_row * 32 + tile_col;
            let tile_id = self.vram_read(map_addr);

            let tile_addr = self.tile_data_addr(tile_id, signed_tiles);
            let color_id = self.tile_pixel(tile_addr, pixel_row, pixel_col);

            bg_index[x] = color_id;
            self.set_pixel(x, line, Self::apply_palette(color_id, self.bgp));
        }
    }

    // WX is stored as (screen X + 7); WX in 0..=6 is a known edge case
    // real hardware handles specially and this simplification doesn't -
    // those columns just won't show window pixels.
    fn render_window(&mut self, line: usize, bg_index: &mut [u8; SCREEN_WIDTH]) {
        let tile_map_base: u16 = if self.lcdc & 0x40 != 0 { 0x9C00 } else { 0x9800 };
        let signed_tiles = self.lcdc & 0x10 == 0;

        let wx = self.wx.wrapping_sub(7);
        let y = self.window_line;
        let tile_row = (y / 8) as u16;
        let pixel_row = (y % 8) as u16;

        for x in 0..SCREEN_WIDTH {
            let screen_x = x as u8;
            if screen_x < wx {
                continue;
            }

            let window_x = screen_x - wx;
            let tile_col = (window_x / 8) as u16;
            let pixel_col = window_x % 8;

            let map_addr = tile_map_base + tile_row * 32 + tile_col;
            let tile_id = self.vram_read(map_addr);

            let tile_addr = self.tile_data_addr(tile_id, signed_tiles);
            let color_id = self.tile_pixel(tile_addr, pixel_row, pixel_col);

            bg_index[x] = color_id;
            self.set_pixel(x, line, Self::apply_palette(color_id, self.bgp));
        }

        self.window_line = self.window_line.wrapping_add(1);
    }

    fn render_sprites(&mut self, line: usize, bg_index: &[u8; SCREEN_WIDTH]) {
        let sprite_height: u8 = if self.lcdc & 0x04 != 0 { 16 } else { 8 };

        // OAM search: scan entries 0..40 in order and keep the first 10
        // that intersect this line, same limit/order real hardware uses.
        let mut visible = [(0u8, 0usize); 10];
        let mut count = 0;

        for i in 0..40 {
            if count == 10 {
                break;
            }

            let base = i * 4;
            let sprite_y = self.oam[base] as i16 - 16;

            if (line as i16) >= sprite_y && (line as i16) < sprite_y + sprite_height as i16 {
                visible[count] = (self.oam[base + 1], i);
                count += 1;
            }
        }

        let visible = &mut visible[..count];

        // Draw priority: smaller X wins overlapping pixels, ties broken by
        // OAM index. Draw lowest-priority sprites first so higher-priority
        // ones are painted on top last.
        visible.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));

        for &(_, i) in visible.iter() {
            let base = i * 4;
            let sprite_y = self.oam[base] as i16 - 16;
            let sprite_x = self.oam[base + 1] as i16 - 8;
            let mut tile_id = self.oam[base + 2];
            let attrs = self.oam[base + 3];

            let flip_y = attrs & 0x40 != 0;
            let flip_x = attrs & 0x20 != 0;
            let behind_bg = attrs & 0x80 != 0;
            let palette = if attrs & 0x10 != 0 { self.obp1 } else { self.obp0 };

            let mut row = (line as i16 - sprite_y) as u8;
            if flip_y {
                row = sprite_height - 1 - row;
            }

            // In 8x16 mode the low tile-id bit is ignored - the sprite
            // spans a consecutive top/bottom tile pair, and tile_pixel's
            // row*2 addressing below already walks across both of them.
            if sprite_height == 16 {
                tile_id &= 0xFE;
            }
            let tile_addr = 0x8000 + (tile_id as u16) * 16;

            for col in 0..8u8 {
                let screen_x = sprite_x + col as i16;
                if screen_x < 0 || screen_x >= SCREEN_WIDTH as i16 {
                    continue;
                }

                let sample_col = if flip_x { 7 - col } else { col };
                let color_id = self.tile_pixel(tile_addr, row as u16, sample_col);

                if color_id == 0 {
                    continue; // color 0 is always transparent for sprites
                }

                if behind_bg && bg_index[screen_x as usize] != 0 {
                    continue; // hidden behind a non-zero BG/window pixel
                }

                self.set_pixel(screen_x as usize, line, Self::apply_palette(color_id, palette));
            }
        }
    }

    // === Tile decoding helpers ===

    // Internal, ungated VRAM read used by the renderer itself - the CPU's
    // mode-3 lockout in read_vram doesn't apply to the PPU's own access.
    fn vram_read(&self, address: u16) -> u8 {
        self.vram[(address - 0x8000) as usize]
    }

    fn tile_data_addr(&self, tile_id: u8, signed_tiles: bool) -> u16 {
        if signed_tiles {
            // 0x8800 method: tile_id is a signed offset from 0x9000
            let id = tile_id as i8 as i32;
            (0x9000i32 + id * 16) as u16
        } else {
            // 0x8000 method: tile_id is an unsigned offset from 0x8000
            0x8000 + (tile_id as u16) * 16
        }
    }

    // Each tile row is 2 bytes (2 bits per pixel, bitplanes stored
    // separately): low bitplane byte first, then the high bitplane byte.
    fn tile_pixel(&self, tile_addr: u16, row: u16, col: u8) -> u8 {
        let byte_addr = tile_addr + row * 2;
        let low = self.vram_read(byte_addr);
        let high = self.vram_read(byte_addr + 1);

        let bit = 7 - col;
        let lo = (low >> bit) & 1;
        let hi = (high >> bit) & 1;

        (hi << 1) | lo
    }

    // Maps a raw 2-bit color index through a palette register (BGP/OBP0/
    // OBP1) to the shade actually drawn. Each palette packs four 2-bit
    // shade values, one per color index.
    fn apply_palette(color_id: u8, palette: u8) -> u8 {
        (palette >> (color_id * 2)) & 0x03
    }

    fn set_pixel(&mut self, x: usize, y: usize, shade: u8) {
        self.framebuffer[y * SCREEN_WIDTH + x] = shade;
    }
}