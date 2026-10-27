//! Tiny animation helpers: damped springs and the pet's hop-and-squish motion.

/// A critically-or-under-damped 1-D spring.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub stiffness: f32,
    pub damping: f32,
}

impl Spring {
    pub fn new(value: f32, stiffness: f32, damping: f32) -> Self {
        Spring {
            value,
            velocity: 0.,
            target: value,
            stiffness,
            damping,
        }
    }

    pub fn step(&mut self, dt: f32) {
        // Sub-step for stability with stiff springs and long frames.
        let n = (dt / 0.004).ceil().max(1.) as usize;
        let h = dt / n as f32;
        for _ in 0..n {
            let a = self.stiffness * (self.target - self.value) - self.damping * self.velocity;
            self.velocity += a * h;
            self.value += self.velocity * h;
        }
    }

    pub fn settled(&self) -> bool {
        (self.target - self.value).abs() < 0.01 && self.velocity.abs() < 0.01
    }

    pub fn snap(&mut self, v: f32) {
        self.value = v;
        self.target = v;
        self.velocity = 0.;
    }
}

/// The detached robot: springs toward a target with a little hop, stretching
/// along its motion and squashing when it lands.
pub struct Pet {
    pub x: Spring,
    pub y: Spring,
    /// Landing squash, 0 = none; positive flattens.
    pub squash: Spring,
    hop: Option<f32>,
    moving: bool,
}

impl Pet {
    pub fn new(x: f32, y: f32) -> Self {
        Pet {
            x: Spring::new(x, 42., 11.),
            y: Spring::new(y, 42., 11.),
            squash: Spring::new(0., 320., 9.),
            hop: None,
            moving: false,
        }
    }

    pub fn go(&mut self, x: f32, y: f32) {
        if (x - self.x.target).hypot(y - self.y.target) < 4. {
            return;
        }
        self.x.target = x;
        self.y.target = y;
        self.hop = Some(0.);
        self.moving = true;
        // Crouch before the jump.
        self.squash.velocity += 6.;
    }

    /// Follow a point directly (while being dragged).
    pub fn hold(&mut self, x: f32, y: f32) {
        self.x.target = x;
        self.y.target = y;
        self.hop = None;
        self.moving = true;
    }

    pub fn step(&mut self, dt: f32) {
        self.x.step(dt);
        self.y.step(dt);
        self.squash.step(dt);
        if let Some(t) = self.hop.as_mut() {
            *t += dt / 0.55;
            if *t >= 1. {
                self.hop = None;
            }
        }
        let speed = self.x.velocity.hypot(self.y.velocity);
        if self.moving && speed < 25. && self.hop.is_none() {
            self.moving = false;
            // Land: squash proportional to how fast we were going.
            self.squash.velocity -= 9.;
        }
    }

    pub fn busy(&self) -> bool {
        self.moving || !self.squash.settled() || self.hop.is_some()
    }

    /// Hop height above the spring position, in logical px.
    pub fn lift(&self) -> f32 {
        self.hop
            .map(|t| (std::f32::consts::PI * t).sin() * 26.)
            .unwrap_or(0.)
    }

    /// Horizontal and vertical scale for the squash-and-stretch look.
    pub fn scale(&self) -> (f32, f32) {
        let (vx, vy) = (self.x.velocity, self.y.velocity);
        let speed = vx.hypot(vy);
        let s = (speed / 2200.).min(0.28);
        let (ux, uy) = if speed > 1. {
            ((vx / speed).abs(), (vy / speed).abs())
        } else {
            (0., 0.)
        };
        let q = self.squash.value.clamp(-0.35, 0.35) * 0.06;
        let sx = 1. + s * (ux - 0.6 * uy) + q;
        let sy = 1. + s * (uy - 0.6 * ux) - q;
        (sx.max(0.6), sy.max(0.6))
    }
}

/// Smoothstep-like ease-out for one-shot transitions.
pub fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    1. - (1. - t).powi(3)
}

/// A one-shot bounce: 0 → overshoot → settle at 1 over ~`dur` seconds.
pub fn bounce_in(t: f32) -> f32 {
    if t >= 1. {
        return 1.;
    }
    let t = t.max(0.);
    1. - (-6. * t).exp() * (12. * t).cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_on_target() {
        let mut s = Spring::new(0., 200., 20.);
        s.target = 10.;
        for _ in 0..200 {
            s.step(1. / 60.);
        }
        assert!((s.value - 10.).abs() < 0.05);
    }

    #[test]
    fn pet_reaches_target_and_rests() {
        let mut p = Pet::new(0., 0.);
        p.go(300., 120.);
        for _ in 0..600 {
            p.step(1. / 60.);
        }
        assert!((p.x.value - 300.).abs() < 1.);
        assert!(!p.busy());
        let (sx, sy) = p.scale();
        assert!((sx - 1.).abs() < 0.02 && (sy - 1.).abs() < 0.02);
    }

    #[test]
    fn bounce_ends_at_one() {
        assert_eq!(bounce_in(1.), 1.);
        assert!(bounce_in(0.2) > 0.9); // overshoots early
    }
}
