// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! AMS-81 fold-down: a receiver's channels (up to 8, e.g. a 32ME) to one stereo pair,
//! after the Ward-Beck AMS8-1A monitor. INPUTS 1/2 · 3/4 · 5/6 · 7/8 pick a pair; PRESETS
//! A–D are stored L / R mixes (each channel −15…+15 dB or off per side); Lo/Ro folds
//! L R C LFE Ls Rs down to stereo (the AMS8-1A MODE1 downmix). The receiver writes the
//! result as its own stereo ring, so everything downstream sees a plain stereo flow.

use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

pub const CHANNELS: usize = 8;
pub const GAIN_MIN: f64 = -15.0;
pub const GAIN_MAX: f64 = 15.0;
const PRESETS: [&str; 4] = ["A", "B", "C", "D"];
const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// What the fold-down plays.
#[derive(Clone, Debug, PartialEq)]
pub enum Select {
    /// 0..3 = channels 1/2 … 7/8
    Pair(usize),
    /// 0..3 = presets A … D
    Preset(usize),
    LoRo,
}

/// One preset: per side, a gain in dB per channel (None = off).
pub type Mix = [[Option<f64>; CHANNELS]; 2];

#[derive(Clone, Debug)]
pub struct Ams81 {
    pub select: Select,
    pub presets: [Mix; 4],
}

impl Default for Ams81 {
    /// Pair 1/2; preset k starts as pair k (as the AMS8-1A simulator infers).
    fn default() -> Self {
        let preset = |k: usize| {
            let mut m: Mix = [[None; CHANNELS]; 2];
            m[0][2 * k] = Some(0.0);
            m[1][2 * k + 1] = Some(0.0);
            m
        };
        Ams81 { select: Select::Pair(0), presets: [preset(0), preset(1), preset(2), preset(3)] }
    }
}

impl Ams81 {
    /// Fold planar channels to [L, R].
    pub fn fold(&self, planar: &[Vec<f32>]) -> [Vec<f32>; 2] {
        let n = planar.first().map(|c| c.len()).unwrap_or(0);
        let ch = |i: usize| planar.get(i);
        let mut out = [vec![0.0f32; n], vec![0.0f32; n]];
        let mut add = |side: usize, i: usize, g: f32| {
            if let Some(src) = ch(i) {
                for (o, s) in out[side].iter_mut().zip(src) {
                    *o += g * s;
                }
            }
        };
        match &self.select {
            Select::Pair(k) => {
                add(0, 2 * k, 1.0);
                add(1, 2 * k + 1, 1.0);
            }
            Select::Preset(p) => {
                for (side, gains) in self.presets[*p].iter().enumerate() {
                    for (i, g) in gains.iter().enumerate() {
                        if let Some(db) = g {
                            add(side, i, 10f64.powf(db / 20.0) as f32);
                        }
                    }
                }
            }
            Select::LoRo => {
                // L R C LFE Ls Rs (7 / 8 ride with the surrounds); LFE is left out
                add(0, 0, 1.0);
                add(1, 1, 1.0);
                add(0, 2, M3DB);
                add(1, 2, M3DB);
                add(0, 4, M3DB);
                add(1, 5, M3DB);
                add(0, 6, M3DB);
                add(1, 7, M3DB);
            }
        }
        out
    }

    pub fn select_name(&self) -> String {
        match &self.select {
            Select::Pair(k) => format!("{}/{}", 2 * k + 1, 2 * k + 2),
            Select::Preset(p) => PRESETS[*p].to_string(),
            Select::LoRo => "Lo/Ro".into(),
        }
    }

    /// "1/2" … "7/8", "A" … "D", "LoRo"
    pub fn parse_select(s: &str) -> Option<Select> {
        if let Some(p) = PRESETS.iter().position(|n| n.eq_ignore_ascii_case(s)) {
            return Some(Select::Preset(p));
        }
        if s.eq_ignore_ascii_case("loro") || s.eq_ignore_ascii_case("lo/ro") {
            return Some(Select::LoRo);
        }
        let first: usize = s.split('/').next()?.trim().parse().ok()?;
        (first % 2 == 1 && first < CHANNELS).then_some(Select::Pair(first / 2))
    }

    pub fn to_json(&self) -> Value {
        let presets: Map<String, Value> = PRESETS
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_string(), json!({"L": self.presets[i][0].to_vec(), "R": self.presets[i][1].to_vec()})))
            .collect();
        json!({"select": self.select_name(), "presets": presets})
    }

    fn from_json(v: &Value) -> Ams81 {
        let mut a = Ams81::default();
        if let Some(s) = v.get("select").and_then(Value::as_str).and_then(Ams81::parse_select) {
            a.select = s;
        }
        for (i, n) in PRESETS.iter().enumerate() {
            for (side, key) in ["L", "R"].iter().enumerate() {
                if let Some(arr) = v.pointer(&format!("/presets/{n}/{key}")).and_then(Value::as_array) {
                    for (c, g) in arr.iter().take(CHANNELS).enumerate() {
                        a.presets[i][side][c] = g.as_f64().map(|d| d.clamp(GAIN_MIN, GAIN_MAX));
                    }
                }
            }
        }
        a
    }

    /// Set one preset gain: preset A–D, side L / R, channel 0..7, dB (None = off).
    pub fn set_gain(&mut self, preset: &str, side: &str, ch: usize, db: Option<f64>) -> Result<(), String> {
        let p = PRESETS.iter().position(|n| n.eq_ignore_ascii_case(preset)).ok_or("preset must be A, B, C or D")?;
        let s = match side {
            "L" | "l" => 0,
            "R" | "r" => 1,
            _ => return Err("side must be L or R".into()),
        };
        if ch >= CHANNELS {
            return Err("channel must be 1 to 8".into());
        }
        self.presets[p][s][ch] = db.map(|d| d.clamp(GAIN_MIN, GAIN_MAX));
        Ok(())
    }
}

/// One fold-down per receiver slot, kept across restarts.
pub fn settings() -> MutexGuard<'static, Vec<Ams81>> {
    static S: Mutex<Vec<Ams81>> = Mutex::new(Vec::new());
    let mut g = S.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_empty() {
        let saved: Option<Value> = std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok());
        *g = (0..crate::SLOTS.len())
            .map(|i| saved.as_ref().and_then(|v| v.get(crate::SLOTS[i].name)).map(Ams81::from_json).unwrap_or_default())
            .collect();
    }
    g
}

pub fn save(all: &[Ams81]) {
    let m: Map<String, Value> = all.iter().enumerate().map(|(i, a)| (crate::SLOTS[i].name.to_string(), a.to_json())).collect();
    let f = file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(f, serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default());
}

fn file() -> PathBuf {
    wbs_log::log_path().with_file_name("ams81.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eight() -> Vec<Vec<f32>> {
        (0..8).map(|c| vec![(c + 1) as f32 / 10.0; 4]).collect()
    }

    #[test]
    fn pairs_presets_loro() {
        let mut a = Ams81 { select: Select::Pair(2), ..Default::default() };
        let [l, r] = a.fold(&eight());
        assert_eq!((l[0], r[0]), (0.5, 0.6));
        a.select = Select::Preset(0);
        a.set_gain("A", "L", 7, Some(-6.0)).unwrap();
        let [l, _] = a.fold(&eight());
        assert!((l[0] - (0.1 + 0.8 * 10f32.powf(-6.0 / 20.0))).abs() < 1e-6);
        a.select = Select::LoRo;
        let [l, _] = a.fold(&eight());
        assert!((l[0] - (0.1 + M3DB * (0.3 + 0.5 + 0.7))).abs() < 1e-6, "LFE out");
        // a stereo source folds to itself on 1/2, silence on 3/4
        let two = vec![vec![0.2f32; 3], vec![0.4f32; 3]];
        a.select = Select::Pair(0);
        assert_eq!(a.fold(&two)[1][0], 0.4);
        a.select = Select::Pair(1);
        assert_eq!(a.fold(&two)[0][0], 0.0);
    }

    #[test]
    fn parse_and_json() {
        assert_eq!(Ams81::parse_select("5/6"), Some(Select::Pair(2)));
        assert_eq!(Ams81::parse_select("c"), Some(Select::Preset(2)));
        assert_eq!(Ams81::parse_select("LoRo"), Some(Select::LoRo));
        assert_eq!(Ams81::parse_select("2/3"), None);
        let mut a = Ams81::default();
        a.set_gain("B", "R", 0, Some(40.0)).unwrap();
        let b = Ams81::from_json(&a.to_json());
        assert_eq!(b.presets[1][1][0], Some(15.0));
        assert_eq!(b.presets[1][1][3], Some(0.0));
    }
}
