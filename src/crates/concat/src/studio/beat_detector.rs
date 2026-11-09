// SPDX-License-Identifier: AGPL-3.0-or-later
//! A streaming attack envelope with extra weight on bass transients.

pub const RATE: u32 = 24_000;
const HOP: usize = RATE as usize / 100;
const HOP_SECONDS: f64 = HOP as f64 / RATE as f64;

pub struct Detector {
    low: [f32; 2],
    mid: [f32; 2],
    low_gain: f32,
    mid_gain: f32,
    energy: [f32; 3],
    previous: [f32; 3],
    filled: usize,
    attacks: Vec<f32>,
}

impl Detector {
    pub fn new() -> Self {
        let gain = |hz: f32| 1.0 - (-std::f32::consts::TAU * hz / RATE as f32).exp();
        Self {
            low: [0.0; 2],
            mid: [0.0; 2],
            low_gain: gain(150.0),
            mid_gain: gain(1800.0),
            energy: [0.0; 3],
            previous: [0.0; 3],
            filled: 0,
            attacks: Vec::new(),
        }
    }

    pub fn push(&mut self, samples: [i16; 2]) {
        for (channel, sample) in samples.into_iter().enumerate() {
            let sample = f32::from(sample) / 32768.0;
            self.low[channel] += self.low_gain * (sample - self.low[channel]);
            self.mid[channel] += self.mid_gain * (sample - self.mid[channel]);
            self.energy[0] += self.low[channel].powi(2);
            self.energy[1] += (self.mid[channel] - self.low[channel]).powi(2);
            self.energy[2] += (sample - self.mid[channel]).powi(2);
        }
        self.filled += 1;
        if self.filled == HOP {
            self.fold_hop();
        }
    }

    fn fold_hop(&mut self) {
        let mut attack = 0.0;
        for (band, weight) in [1.0, 0.65, 0.25].into_iter().enumerate() {
            let rms = (self.energy[band] / (2 * self.filled) as f32).sqrt();
            let level = (1.0 + 20.0 * rms).ln();
            attack += weight * (level - self.previous[band]).max(0.0);
            self.previous[band] = level;
        }
        self.attacks.push(attack);
        self.energy = [0.0; 3];
        self.filled = 0;
    }

    pub fn finish(mut self) -> Vec<f64> {
        if self.filled > 0 {
            self.fold_hop();
        }
        let attacks = &self.attacks;
        let mut hits: Vec<usize> = Vec::new();
        for (index, &attack) in attacks.iter().enumerate() {
            let left = index.saturating_sub(30);
            let right = (index + 31).min(attacks.len());
            let window = &attacks[left..right];
            let average = window.iter().sum::<f32>() / window.len() as f32;
            if attack < (2.5 * average).max(0.065)
                || index.checked_sub(1).is_some_and(|i| attacks[i] >= attack)
                || attacks.get(index + 1).is_some_and(|next| *next > attack)
            {
                continue;
            }
            // Keep one attack per 240 ms, favouring the stronger of nearby
            // attacks (for instance a drum followed by its room echo).
            if let Some(last) = hits.last_mut()
                && index - *last < 24
            {
                if attack > attacks[*last] {
                    *last = index;
                }
                continue;
            }
            hits.push(index);
        }
        hits.into_iter().map(|i| i as f64 * HOP_SECONDS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_drum_attacks_with_steady_background() {
        let mut detector = Detector::new();
        let starts = [0.19, 0.69, 1.19, 1.69];
        for index in 0..RATE * 2 {
            let time = f64::from(index) / f64::from(RATE);
            let mut sample = (std::f64::consts::TAU * 330.0 * time).sin() * 0.06;
            for start in starts {
                let offset = time - start;
                if (0.0..0.09).contains(&offset) {
                    sample += (std::f64::consts::TAU * 65.0 * offset).sin()
                        * 0.65
                        * (-30.0 * offset).exp();
                }
            }
            let sample = (sample * 32767.0) as i16;
            detector.push([sample, -sample]);
        }
        let beats = detector.finish();
        assert_eq!(beats.len(), starts.len(), "{beats:?}");
        for (beat, expected) in beats.into_iter().zip(starts) {
            assert!((beat - expected).abs() <= 0.02, "{beat} versus {expected}");
        }
    }

    #[test]
    fn silence_has_no_beats() {
        let mut detector = Detector::new();
        for _ in 0..RATE {
            detector.push([0, 0]);
        }
        assert!(detector.finish().is_empty());
    }
}
