use esp_hal::gpio::{Level, Output, OutputConfig};

pub struct RgbLed<'a> {
    pub r: Output<'a>,
    pub g: Output<'a>,
    pub b: Output<'a>,
}

impl<'a> RgbLed<'a> {
    pub fn new(
        r: impl esp_hal::gpio::OutputPin + 'a,
        g: impl esp_hal::gpio::OutputPin + 'a,
        b: impl esp_hal::gpio::OutputPin + 'a,
    ) -> Self {
        Self {
            r: Output::new(r, Level::Low, OutputConfig::default()),
            g: Output::new(g, Level::Low, OutputConfig::default()),
            b: Output::new(b, Level::Low, OutputConfig::default()),
        }
    }

    pub fn set(&mut self, r: bool, g: bool, b: bool) {
        if r { self.r.set_high(); } else { self.r.set_low(); }
        if g { self.g.set_high(); } else { self.g.set_low(); }
        if b { self.b.set_high(); } else { self.b.set_low(); }
    }

    pub fn off(&mut self) {
        self.set(false, false, false);
    }

    /// CO2 уровень → цвет
    ///
    /// - 0-700 ppm: зелёный (норма)
    /// - 700-1000 ppm: жёлтый (R+G, приемлемо)
    /// - 1000+ ppm: красный (плохо)
    pub fn set_co2(&mut self, co2: u16) {
        match co2 {
            0..=700 => self.set(false, true, false),
            701..=1000 => self.set(true, true, false),
            _ => self.set(true, false, false),
        }
    }

    /// Тест при старте: R → G → B → белый → выкл
    pub fn startup_test(&mut self) {
        use esp_hal::delay::Delay;
        use esp_println::println;

        let delay = Delay::new();

        let sequence: [(bool, bool, bool, &str); 5] = [
            (true, false, false, "RED"),
            (false, true, false, "GREEN"),
            (false, false, true, "BLUE"),
            (true, true, true, "WHITE"),
            (false, false, false, "OFF"),
        ];

        for (r, g, b, name) in sequence {
            println!("LED test: {}", name);
            self.set(r, g, b);
            delay.delay_millis(500);
        }
    }
}
