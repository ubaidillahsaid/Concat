// SPDX-License-Identifier: AGPL-3.0-or-later
//! Detect prominent rhythmic attacks and map their source times onto clips.

use super::*;
use concat_media::{AudioDecoder, AudioOptions};
use std::fmt::Write;

#[path = "beat_detector.rs"]
mod detector;

type Source = (String, Option<u32>);

#[derive(Default)]
pub(super) struct Beats {
    detected: HashMap<Source, Vec<f64>>,
    pending: HashSet<Source>,
}

impl Studio {
    fn beat_source(&self, clip: &Clip) -> Option<Source> {
        if clip.kind != model::ClipKind::Audio {
            return None;
        }
        let media = self.project().media_by_id(&clip.media_id)?;
        Some((media.path.clone(), clip.audio_stream))
    }

    pub(super) fn beat_menu_state(&self, clip: &Clip) -> Option<(bool, bool)> {
        let source = self.beat_source(clip)?;
        Some((
            self.beats.pending.contains(&source),
            self.beats.detected.contains_key(&source),
        ))
    }

    pub(super) fn detect_beats(&mut self, clip: &Clip) {
        let Some(source) = self.beat_source(clip) else {
            return;
        };
        if !self.beats.pending.insert(source.clone()) {
            return;
        }
        self.notify(&t("studio.detectingBeats"), false);
        crate::host::spawn_in_project(
            move || {
                let result = scan(&source.0, source.1);
                (source, result)
            },
            |studio, _, _, (source, result)| {
                if !studio.beats.pending.remove(&source) {
                    return;
                }
                match result {
                    Ok(times) if times.is_empty() => {
                        studio.beats.detected.remove(&source);
                        studio.notify(&t("studio.noBeatsFound"), false);
                    }
                    Ok(times) => {
                        let count = times.len();
                        studio.beats.detected.insert(source, times);
                        studio.notify(&tf("studio.beatsFound", &[&count]), false);
                    }
                    Err(error) => {
                        log::warn!("beat detection: {error}");
                        studio.notify(&t("studio.beatDetectionFailed"), true);
                    }
                }
            },
        );
    }

    pub(super) fn clear_beats(&mut self, clip: &Clip) {
        if let Some(source) = self.beat_source(clip)
            && !self.beats.pending.contains(&source)
        {
            self.beats.detected.remove(&source);
        }
    }

    /// Each hit belongs to the recording. Trims, splits and retiming show
    /// the part of that recording the clip actually uses.
    pub(super) fn beat_times(
        &self,
        clip: &Clip,
        window: Option<(f32, f32)>,
    ) -> impl Iterator<Item = f32> + '_ {
        let source = self.beat_source(clip);
        let beats = source
            .as_ref()
            .and_then(|source| self.beats.detected.get(source))
            .map_or(&[][..], Vec::as_slice);
        let start = clip.start;
        let duration = clip.duration;
        let source_start = clip.source_start;
        let speed = clip.speed;
        let curve = clip
            .speed_curve
            .as_deref()
            .filter(|_| !beats.is_empty())
            .and_then(concat_project::speed::curve_of);
        let (left, right) = window
            .map(|(left, right)| (f64::from(left), f64::from(right)))
            .unwrap_or((start, start + duration));
        let source_at = |time: f64| {
            let elapsed = (time - start).clamp(0.0, duration);
            source_start
                + curve.as_ref().map_or(elapsed * speed, |curve| {
                    duration * curve.consumed(elapsed / duration)
                })
        };
        let from = source_at(left);
        let to = source_at(right);
        let end = source_at(start + duration);
        let first = beats.partition_point(|time| *time <= source_start || *time < from);
        let last = beats
            .partition_point(|time| *time < end && *time <= to)
            .max(first);

        beats[first..last].iter().map(move |&time| {
            let offset = time - source_start;
            let elapsed = if let Some(curve) = &curve {
                let (mut lo, mut hi) = (0.0, 1.0);
                for _ in 0..24 {
                    let mid = (lo + hi) * 0.5;
                    if duration * curve.consumed(mid) < offset {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                duration * (lo + hi) * 0.5
            } else {
                offset / speed
            };
            (start + elapsed) as f32
        })
    }

    pub(super) fn beat_path(&self, clip: &Clip, span: Option<(f32, f32)>) -> SharedString {
        let start = clip.start as f32;
        let duration = clip.duration as f32;
        let pixel = self.lanes.seconds_per_pixel;
        let mut path = String::new();
        let mut last = f32::NEG_INFINITY;
        for time in self.beat_times(clip, span) {
            if time - last < 4.0 * pixel {
                continue;
            }
            let x = (time - start) / duration;
            let width = pixel / duration;
            let from = (x - width).max(0.0);
            let to = (x + width).min(1.0);
            let _ = write!(path, "M{from:.7},1 L{x:.7},0 L{to:.7},1 Z ");
            last = time;
        }
        path.into()
    }
}

fn scan(path: &str, stream: Option<u32>) -> concat_media::error::Result<Vec<f64>> {
    let mut audio = AudioDecoder::open(
        std::path::Path::new(path),
        &AudioOptions {
            rate: detector::RATE,
            channels: 2,
            stream: stream.map(|index| index as usize),
            ..AudioOptions::default()
        },
    )?;
    let mut detector = detector::Detector::new();
    while let Some(samples) = audio.next_i16()? {
        for pair in samples.chunks_exact(2) {
            detector.push([pair[0], pair[1]]);
        }
    }
    Ok(detector.finish())
}
