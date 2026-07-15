pub struct ScrollTracker {
    tick: f64,
    last: f64,
    acc: f64,
}

impl ScrollTracker {
    #[allow(clippy::new_without_default)]
    #[must_use]
    pub fn new(tick: i8) -> Self {
        Self {
            tick: f64::from(tick),
            last: 0.,
            acc: 0.,
        }
    }

    pub fn accumulate(&mut self, amount: f64) -> i8 {
        let changed_direction = (self.last > 0. && amount < 0.) || (self.last < 0. && amount > 0.);
        if changed_direction {
            self.acc = 0.;
        }

        self.last = amount;
        self.acc += amount;

        let mut ticks = 0;
        if self.acc.abs() >= self.tick {
            let clamped = self.acc.clamp(-127. * self.tick, 127. * self.tick);
            // `clamped` is bounded to ±127 * self.tick and `self.tick` originates from an
            // i8 (see `new`), so the i16 division result always fits in -127..=127 (i8).
            #[allow(clippy::cast_possible_truncation)]
            {
                ticks = (clamped as i16 / self.tick as i16) as i8;
            }
            self.acc %= self.tick;
        }

        ticks
    }

    pub const fn reset(&mut self) {
        self.last = 0.;
        self.acc = 0.;
    }
}
