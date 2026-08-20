// Game Boy joypad / controller.
// The Game Boy exposes the controller through register 0xFF00.
// Bits 4 and 5 select which group of buttons is being queried:

// Bit 4 = 0 -> directional buttons
// Bit 5 = 0 -> A, B, Select, Start

// Bits 0-3 are active-low:
// 0 means the button is pressed.
// 1 means the button is released.

#[derive(Clone, Copy)]
pub enum Button {
    Right,
    Left,
    Up,
    Down,
    A,
    B,
    Select,
    Start,
}

pub struct Joypad {
    // Current state of all eight buttons.
    // true = pressed, false = released.
    buttons: [bool; 8],

    // Joypad register (0xFF00).
    //
    // Bits 4 and 5 are controlled by the CPU.
    // Bits 0-3 are generated from the currently selected buttons.
    select: u8,
}

impl Joypad {
    pub fn new() -> Self {
        Self {
            buttons: [false; 8],
            select: 0x30,
        }
    }

    // Called when a button is pressed. Returns true only on the
    // released->pressed edge, so callers (Bus) know when it's actually a
    // new press worth requesting the joypad interrupt for - main.rs polls
    // key state every frame, so this gets called repeatedly the whole
    // time a key is held.
    pub fn press(&mut self, button: Button) -> bool {
        let was_pressed = self.buttons[button as usize];
        self.buttons[button as usize] = true;
        !was_pressed
    }

    // Called when a button is released.
    pub fn release(&mut self, button: Button) {
        self.buttons[button as usize] = false;
    }

    // Reads the value visible to the CPU at 0xFF00.
    pub fn read(&self) -> u8 {
        // Bits 0-3 are active-low: 1 means "not pressed". They must start
        // set, since we only ever clear bits below for buttons that are
        // both selected and actually held - leaving them at 0 by default
        // (as `self.select | 0xC0` alone would) makes every button look
        // permanently pressed, even when neither group is selected, which
        // is exactly what stalls games waiting for "no buttons held"
        // (e.g. the Pokémon Red logo screen) and makes real presses do
        // nothing since the game already thinks everything is held.
        let mut result = self.select | 0xC0 | 0x0F;

        // Select directional buttons.
        if self.select & 0x10 == 0 {
            if self.buttons[Button::Right as usize] {
                result &= !0x01;
            }

            if self.buttons[Button::Left as usize] {
                result &= !0x02;
            }

            if self.buttons[Button::Up as usize] {
                result &= !0x04;
            }

            if self.buttons[Button::Down as usize] {
                result &= !0x08;
            }
        }

        // Select A/B/Select/Start.
        if self.select & 0x20 == 0 {
            if self.buttons[Button::A as usize] {
                result &= !0x01;
            }

            if self.buttons[Button::B as usize] {
                result &= !0x02;
            }

            if self.buttons[Button::Select as usize] {
                result &= !0x04;
            }

            if self.buttons[Button::Start as usize] {
                result &= !0x08;
            }
        }

        result
    }

    // CPU writes bits 4 and 5 to select which button group it wants.
    pub fn write(&mut self, value: u8) {
        self.select = value & 0x30;
    }

    pub fn is_pressed(&self, button: Button) -> bool {
        self.buttons[button as usize]
    }
}