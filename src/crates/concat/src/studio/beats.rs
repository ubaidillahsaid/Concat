// SPDX-License-Identifier: AGPL-3.0-or-later
//! Detect prominent rhythmic attacks and map their source times onto clips.

use super::*;
use concat_media::{AudioDecoder, AudioOptions};

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

    fn selected_still_ids(&self) -> impl Iterator<Item = &str> {
        self.project()
            .media
            .iter()
            .filter(|item| {
                item.kind == model::MediaKind::Image && self.media.selected.contains(&item.id)
            })
            .map(|item| item.id.as_str())
    }

    pub(super) fn has_selected_stills(&self) -> bool {
        self.selected_still_ids().next().is_some()
    }

    pub(super) fn sync_stills_to_beats(&mut self, audio: &Clip) {
        if self.beat_menu_state(audio) != Some((false, true)) {
            return;
        }
        let media_ids = self
            .selected_still_ids()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if media_ids.is_empty() {
            return;
        }
        let rate = self.timeline().video.rate();
        let min_span = (1.0 / rate).max(model::ranges::MIN_CLIP_DURATION);
        let end = audio.start + audio.duration;
        let mut previous = audio.start;
        let mut boundaries = vec![previous];
        for beat in self.beat_times(audio, None) {
            let cut = (f64::from(beat) * rate).round() / rate;
            if cut - previous >= min_span - 1e-9 && end - cut >= min_span - 1e-9 {
                boundaries.push(cut);
                previous = cut;
                // The beat after the last photo ends the sequence.
                if boundaries.len() > media_ids.len() {
                    break;
                }
            }
        }
        if boundaries.len() < media_ids.len() {
            self.notify(&t("studio.notEnoughBeatsForPhotos"), true);
            return;
        }
        if boundaries.len() == media_ids.len() {
            boundaries.push(end);
        }
        if let Some(id) = self.apply(Command::AddStillsAtCuts {
            media_ids,
            boundaries,
        }) {
            self.selection = vec![id];
            self.transition_selected = None;
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

    /// Marker positions as fractions of the clip width. At wide zoom
    /// levels only draw marks with room for a dot; all beats still snap.
    pub(super) fn beat_markers(&self, clip: &Clip, span: Option<(f32, f32)>) -> Vec<f32> {
        let start = clip.start as f32;
        let duration = clip.duration as f32;
        let spacing = 10.0 * self.lanes.seconds_per_pixel;
        let mut markers = Vec::new();
        let mut last = f32::NEG_INFINITY;
        for time in self.beat_times(clip, span) {
            if time - last >= spacing {
                markers.push((time - start) / duration);
                last = time;
            }
        }
        markers
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
