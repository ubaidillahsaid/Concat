// SPDX-License-Identifier: AGPL-3.0-or-later
//! Detect prominent rhythmic attacks and map their source times onto clips.

use super::*;
use concat_media::{AudioDecoder, AudioOptions};

#[path = "beat_detector.rs"]
mod detector;

type Source = (String, Option<u32>);

#[derive(Default)]
pub(super) struct Beats {
    pending: HashSet<Source>,
}

impl Studio {
    fn beat_source(&self, clip: &Clip) -> Option<Source> {
        if clip.kind != model::ClipKind::Audio {
            return None;
        }
        let media = self.project().media_by_id(&clip.media_id)?;
        Some((media.id.clone(), clip.audio_stream))
    }

    pub(super) fn beat_menu_state(&self, clip: &Clip) -> Option<(bool, bool)> {
        let source = self.beat_source(clip)?;
        Some((
            self.beats.pending.contains(&source),
            self.project()
                .media_by_id(&source.0)
                .and_then(|media| media.beat_markers(source.1))
                .is_some(),
        ))
    }

    pub(super) fn detect_beats(&mut self, clip: &Clip) {
        let Some(source) = self.beat_source(clip) else {
            return;
        };
        let Some(media) = self.project().media_by_id(&source.0) else {
            return;
        };
        let path = media.path.clone();
        let before = media.beat_markers(source.1);
        if !self.beats.pending.insert(source.clone()) {
            return;
        }
        self.notify(&t("studio.detectingBeats"), false);
        crate::host::spawn_in_project(
            move || {
                let result = scan(&path, source.1);
                (source, path, before, result)
            },
            |studio, _, _, (source, path, before, result)| {
                if !studio.beats.pending.remove(&source) {
                    return;
                }
                // Results for replaced media, or marks edited while a worker
                // was running, cannot overwrite the current document.
                let unchanged = studio
                    .project()
                    .media_by_id(&source.0)
                    .is_some_and(|media| {
                        media.path == path && media.beat_markers(source.1) == before
                    });
                if !unchanged {
                    return;
                }
                match result {
                    Ok(times) => {
                        let count = times.len();
                        studio.apply(Command::SetMediaBeatMarkers {
                            media_id: source.0,
                            stream: source.1,
                            times: Some(times),
                        });
                        if count == 0 {
                            studio.notify(&t("studio.noBeatsFound"), false);
                        } else {
                            studio.notify(&tf("studio.beatsFound", &[&count]), false);
                        }
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
            self.apply(Command::SetMediaBeatMarkers {
                media_id: source.0,
                stream: source.1,
                times: None,
            });
        }
    }

    /// One release makes one undoable edit; previewing the drag stays in Slint.
    pub fn edit_beat_marker(&mut self, id: &str, source_time: &str, seconds: f32, remove: bool) {
        let Some(clip) = self.clip(id).cloned() else {
            return;
        };
        if self.locked(&clip.track_id) {
            return;
        }
        let Some(source) = self.beat_source(&clip) else {
            return;
        };
        if self.beats.pending.contains(&source) {
            return;
        }
        let Ok(from) = source_time.parse::<f64>() else {
            return;
        };
        let rate = self.timeline().video.rate();
        let latest = (clip.start + clip.duration - 1.0 / rate).max(clip.start);
        let time = (f64::from(seconds) * rate).round() / rate;
        let to = (!remove).then(|| source_at(&clip, time.clamp(clip.start, latest)));
        self.apply(Command::EditMediaBeatMarker {
            media_id: source.0,
            stream: source.1,
            from,
            to,
        });
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

    /// Source seconds mapped through this clip's current trim and speed.
    fn beat_points(
        &self,
        clip: &Clip,
        window: Option<(f32, f32)>,
    ) -> impl Iterator<Item = (f32, f64)> {
        let beats = self
            .beat_source(clip)
            .and_then(|source| {
                self.project()
                    .media_by_id(&source.0)?
                    .beat_markers(source.1)
            })
            .unwrap_or_default();
        let start = clip.start;
        let duration = clip.duration;
        let source_start = clip.source_start;
        let speed = clip.speed;
        let curve = clip
            .speed_curve
            .as_deref()
            .and_then(concat_project::speed::curve_of);
        let (left, right) = window
            .map(|(left, right)| (f64::from(left), f64::from(right)))
            .unwrap_or((start, start + duration));
        let from = source_at(clip, left);
        let to = source_at(clip, right);
        let end = source_at(clip, start + duration);
        let first = beats.partition_point(|time| *time < source_start || *time < from);
        let last = beats
            .partition_point(|time| *time < end && *time <= to)
            .max(first);
        beats
            .into_iter()
            .skip(first)
            .take(last - first)
            .map(move |time| {
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
                ((start + elapsed) as f32, time)
            })
    }

    pub(super) fn beat_times(
        &self,
        clip: &Clip,
        window: Option<(f32, f32)>,
    ) -> impl Iterator<Item = f32> {
        self.beat_points(clip, window).map(|(time, _)| time)
    }

    /// Each visible mark carries its exact source instant for editing.
    pub(super) fn beat_markers(
        &self,
        clip: &Clip,
        span: Option<(f32, f32)>,
    ) -> Vec<BeatMarkerData> {
        let start = clip.start as f32;
        let duration = clip.duration as f32;
        self.beat_points(clip, span)
            .map(|(time, source)| BeatMarkerData {
                at: (time - start) / duration,
                source_time: source.to_string().into(),
            })
            .collect()
    }
}

fn source_at(clip: &Clip, timeline_time: f64) -> f64 {
    let elapsed = (timeline_time - clip.start).clamp(0.0, clip.duration);
    let curve = clip
        .speed_curve
        .as_deref()
        .and_then(concat_project::speed::curve_of);
    clip.source_start
        + curve.map_or(elapsed * clip.speed, |curve| {
            clip.duration * curve.consumed(elapsed / clip.duration)
        })
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
