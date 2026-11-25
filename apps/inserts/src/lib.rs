// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.com - Ward-Beck.ca). All rights reserved.
//! Module inserts: Ward-Beck module plugins (M466 Compressor - Limiter, M467 Noise Gate) in a receiver's path, in order, each switched in or out.
//!
//! The DSP of each module comes from that module's own repo (MODULES.<CATEGORY>.<Mxxx>/plugin, the same
//! crate that builds its VST3 / CLAP plugin). The receivers ask `services::inserts()` for every packet;
//! a slot with nothing switched in costs nothing.
//!
//! GET /api/inserts                     every slot's inserts, their settings and meters, and the modules on offer
//! POST /api/inserts {slot, add: "M466"}               add a module at the end of the slot
//! POST /api/inserts {slot, index, remove: true}       take it out
//! POST /api/inserts {slot, index, on: true|false}     switch it in / out (out = bypassed)
//! POST /api/inserts {slot, index, settings: {...}}    change any of its controls
//! Settings are kept in inserts.json beside the log.

mod plugin;
pub use plugin::plug;

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The receivers that can carry inserts.
pub const SLOTS: [&str; 2] = ["network", "moip"];

/// One module's DSP behind a common face.
trait Dsp: Send {
    fn process(&mut self, sample_rate: u32, planar: &mut [Vec<f32>]);
    fn settings(&self) -> Value;
    fn set(&mut self, changes: &Map<String, Value>) -> Result<(), String>;
    fn meters(&self) -> Value;
}

/// A module that can be inserted: its id, what it is, and how to make one.
struct ModuleDef {
    id: &'static str,
    name: &'static str,
    year: u32,
    make: fn() -> Box<dyn Dsp>,
}

const MODULES: &[ModuleDef] = &[
    ModuleDef { id: "M466", name: "Compressor - Limiter", year: 1976, make: || Box::new(m466::Insert::default()) },
    ModuleDef { id: "M467", name: "Noise Gate", year: 1976, make: || Box::new(m467::Insert::default()) },
];

struct Insert {
    module: &'static str,
    on: bool,
    dsp: Box<dyn Dsp>,
}

fn state() -> &'static Mutex<BTreeMap<String, Vec<Insert>>> {
    static S: OnceLock<Mutex<BTreeMap<String, Vec<Insert>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(load()))
}

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<String, Vec<Insert>>> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

fn make(module: &str) -> Option<Insert> {
    let d = MODULES.iter().find(|m| m.id.eq_ignore_ascii_case(module))?;
    Some(Insert { module: d.id, on: true, dsp: (d.make)() })
}

pub fn active(slot: &str) -> bool {
    lock().get(slot).is_some_and(|v| v.iter().any(|i| i.on))
}

pub fn process(slot: &str, sample_rate: u32, planar: &mut [Vec<f32>]) {
    let mut s = lock();
    if let Some(chain) = s.get_mut(slot) {
        for i in chain.iter_mut().filter(|i| i.on) {
            i.dsp.process(sample_rate, planar);
        }
    }
}

pub fn status() -> Value {
    let s = lock();
    let slots: Map<String, Value> = SLOTS
        .iter()
        .map(|slot| {
            let chain = s.get(*slot).map(|v| v.iter().map(|i| json!({"module": i.module, "on": i.on, "settings": i.dsp.settings(), "meters": i.dsp.meters()})).collect()).unwrap_or_else(Vec::new);
            (slot.to_string(), Value::Array(chain))
        })
        .collect();
    let modules: Vec<Value> = MODULES.iter().map(|m| json!({"id": m.id, "name": m.name, "year": m.year})).collect();
    json!({"modules": modules, "slots": slots})
}

/// Apply one POST; the reply is the whole status.
pub fn post(d: &Map<String, Value>) -> Result<Value, String> {
    let slot = d.get("slot").and_then(Value::as_str).unwrap_or_default().to_string();
    if !SLOTS.contains(&slot.as_str()) {
        return Err(format!("slot must be one of {}, not '{slot}'", SLOTS.join(", ")));
    }
    {
        let mut s = lock();
        let chain = s.entry(slot.clone()).or_default();
        if let Some(add) = d.get("add").and_then(Value::as_str) {
            let ins = make(add).ok_or_else(|| format!("no module '{add}' (have {})", MODULES.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")))?;
            wbs_log::info("inserts", &format!("{slot}: {} inserted at position {}", ins.module, chain.len() + 1));
            chain.push(ins);
        } else {
            let idx = d.get("index").and_then(Value::as_u64).ok_or("index is missing")? as usize;
            if idx >= chain.len() {
                return Err(format!("{slot} has {} insert(s); there is no index {idx}", chain.len()));
            }
            if d.get("remove").map(wbs_pyshim::truthy).unwrap_or(false) {
                let gone = chain.remove(idx);
                wbs_log::info("inserts", &format!("{slot}: {} removed", gone.module));
            } else {
                if let Some(on) = d.get("on") {
                    chain[idx].on = wbs_pyshim::truthy(on);
                    wbs_log::info("inserts", &format!("{slot}: {} switched {}", chain[idx].module, if chain[idx].on { "in" } else { "out" }));
                }
                if let Some(Value::Object(ch)) = d.get("settings") {
                    chain[idx].dsp.set(ch).inspect_err(|e| wbs_log::warn("inserts", &format!("{slot}: {}: {e}", chain[idx].module)))?;
                }
            }
        }
    }
    save();
    Ok(status())
}

fn file() -> PathBuf {
    wbs_log::log_path().with_file_name("inserts.json")
}

fn save() {
    let s = lock();
    let m: Map<String, Value> = s.iter().map(|(k, v)| (k.clone(), Value::Array(v.iter().map(|i| json!({"module": i.module, "on": i.on, "settings": i.dsp.settings()})).collect()))).collect();
    let f = file();
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = std::fs::write(&f, serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default()) {
        wbs_log::warn("inserts", &format!("could not save {}: {e}", f.display()));
    }
}

fn load() -> BTreeMap<String, Vec<Insert>> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(file()) else { return out };
    let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&text) else {
        wbs_log::warn("inserts", "inserts.json is not readable; starting with no inserts");
        return out;
    };
    for (slot, list) in m {
        let chain: Vec<Insert> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let mut i = make(v.get("module")?.as_str()?)?;
                i.on = v.get("on").map(wbs_pyshim::truthy).unwrap_or(true);
                if let Some(Value::Object(s)) = v.get("settings") {
                    let _ = i.dsp.set(s);
                }
                Some(i)
            })
            .collect();
        if !chain.is_empty() {
            wbs_log::info("inserts", &format!("{slot}: {}", chain.iter().map(|i| format!("{}{}", i.module, if i.on { "" } else { " (out)" })).collect::<Vec<_>>().join(" → ")));
        }
        out.insert(slot, chain);
    }
    out
}

/// M466 Compressor - Limiter (MODULES.DYNAMICS.M466).
mod m466 {
    use super::Dsp;
    use serde_json::{json, Map, Value};
    use wbs_m466::dsp::{Settings, M466};

    pub struct Insert {
        rate: u32,
        dsp: M466,
    }

    impl Default for Insert {
        fn default() -> Self {
            Insert { rate: 48000, dsp: M466::new(48000.0) }
        }
    }

    impl Dsp for Insert {
        fn process(&mut self, sample_rate: u32, planar: &mut [Vec<f32>]) {
            if sample_rate != self.rate && sample_rate > 0 {
                self.rate = sample_rate;
                self.dsp.set_sample_rate(sample_rate as f32);
            }
            let mut chans: Vec<&mut [f32]> = planar.iter_mut().map(|c| c.as_mut_slice()).collect();
            self.dsp.process(&mut chans);
        }

        fn settings(&self) -> Value {
            let s = self.dsp.settings();
            json!({
                "insert": s.insert, "comp": s.comp, "lim": s.lim, "de_ess": s.de_ess, "meter_vu": s.meter_vu,
                "sens_db": s.sens_db, "comp_thld_db": s.comp_thld_db, "slope": s.slope, "comp_rls_s": s.comp_rls_s,
                "lim_thld_db": s.lim_thld_db, "lim_rls_s": s.lim_rls_s, "ds_hz": s.ds_hz,
            })
        }

        fn set(&mut self, c: &Map<String, Value>) -> Result<(), String> {
            let mut s: Settings = self.dsp.settings();
            for (k, v) in c {
                let num = |lo: f32, hi: f32| -> Result<f32, String> {
                    let x = v.as_f64().ok_or_else(|| format!("{k} must be a number"))? as f32;
                    if !(lo..=hi).contains(&x) {
                        return Err(format!("{k} {x} is outside {lo} … {hi}"));
                    }
                    Ok(x)
                };
                let flag = || v.as_bool().ok_or_else(|| format!("{k} must be true or false"));
                match k.as_str() {
                    "insert" => s.insert = flag()?,
                    "comp" => s.comp = flag()?,
                    "lim" => s.lim = flag()?,
                    "de_ess" => s.de_ess = flag()?,
                    "meter_vu" => s.meter_vu = flag()?,
                    "sens_db" => s.sens_db = num(-8.0, 10.0)?,
                    "comp_thld_db" => s.comp_thld_db = num(-20.0, 0.0)?,
                    "slope" => s.slope = num(1.5, 10.0)?,
                    "comp_rls_s" => s.comp_rls_s = num(0.1, 5.0)?,
                    "lim_thld_db" => s.lim_thld_db = num(0.0, 20.0)?,
                    "lim_rls_s" => s.lim_rls_s = num(0.1, 5.0)?,
                    "ds_hz" => s.ds_hz = num(1000.0, 8000.0)?,
                    _ => return Err(format!("M466 has no control '{k}'")),
                }
            }
            self.dsp.set(s);
            Ok(())
        }

        fn meters(&self) -> Value {
            let m = self.dsp.meters;
            json!({"comp_gr_db": m.comp_gr_db, "lim_gr_db": m.lim_gr_db, "led6": m.led6, "led12": m.led12,
                   "comp_led": m.comp_led, "lim_led": m.lim_led, "out_dbfs": m.out_dbfs})
        }
    }
}

/// M467 Noise Gate (MODULES.DYNAMICS.M467).
mod m467 {
    use super::Dsp;
    use serde_json::{json, Map, Value};
    use wbs_m467::dsp::{Settings, M467};

    pub struct Insert {
        rate: u32,
        dsp: M467,
    }

    impl Default for Insert {
        fn default() -> Self {
            Insert { rate: 48000, dsp: M467::new(48000.0) }
        }
    }

    impl Dsp for Insert {
        fn process(&mut self, sample_rate: u32, planar: &mut [Vec<f32>]) {
            if sample_rate != self.rate && sample_rate > 0 {
                self.rate = sample_rate;
                self.dsp.set_sample_rate(sample_rate as f32);
            }
            let mut chans: Vec<&mut [f32]> = planar.iter_mut().map(|c| c.as_mut_slice()).collect();
            self.dsp.process(&mut chans);
        }

        fn settings(&self) -> Value {
            let s = self.dsp.settings();
            json!({"gate": s.gate, "thld_db": s.thld_db, "atten_db": s.atten_db, "rls_s": s.rls_s, "fast": s.fast, "comp_tie": s.comp_tie})
        }

        fn set(&mut self, c: &Map<String, Value>) -> Result<(), String> {
            let mut s: Settings = self.dsp.settings();
            for (k, v) in c {
                let num = |lo: f32, hi: f32| -> Result<f32, String> {
                    let x = v.as_f64().ok_or_else(|| format!("{k} must be a number"))? as f32;
                    if !(lo..=hi).contains(&x) {
                        return Err(format!("{k} {x} is outside {lo} … {hi}"));
                    }
                    Ok(x)
                };
                let flag = || v.as_bool().ok_or_else(|| format!("{k} must be true or false"));
                match k.as_str() {
                    "gate" => s.gate = flag()?,
                    "fast" => s.fast = flag()?,
                    "thld_db" => s.thld_db = num(-50.0, 0.0)?,
                    "atten_db" => s.atten_db = num(0.0, 50.0)?,
                    "rls_s" => s.rls_s = num(0.1, 5.0)?,
                    "comp_tie" => s.comp_tie = num(0.0, 2.0)? as u8,
                    _ => return Err(format!("M467 has no control '{k}'")),
                }
            }
            self.dsp.set(s);
            Ok(())
        }

        fn meters(&self) -> Value {
            let m = self.dsp.meters;
            json!({"open": m.open, "atten_db": m.atten_db, "vu": m.vu})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_set_switch_remove() {
        let p = |v: Value| post(v.as_object().unwrap());
        assert!(p(json!({"slot": "nowhere", "add": "M466"})).is_err());
        assert!(p(json!({"slot": "moip", "add": "M999"})).is_err());
        let st = p(json!({"slot": "moip", "add": "M466"})).unwrap();
        assert_eq!(st["slots"]["moip"][0]["module"], "M466");
        assert!(active("moip"));
        assert!(p(json!({"slot": "moip", "index": 0, "settings": {"sens_db": 40.0}})).is_err());
        let st = p(json!({"slot": "moip", "index": 0, "settings": {"lim_thld_db": 0.0, "comp": false}})).unwrap();
        assert_eq!(st["slots"]["moip"][0]["settings"]["lim_thld_db"], 0.0);
        // a hot packet is limited
        let mut pk = vec![vec![0.8f32; 4800], vec![0.8f32; 4800]];
        process("moip", 48000, &mut pk);
        assert!(pk[0][4799] < 0.2, "limited, got {}", pk[0][4799]);
        p(json!({"slot": "moip", "index": 0, "on": false})).unwrap();
        assert!(!active("moip"));
        p(json!({"slot": "moip", "index": 0, "remove": true})).unwrap();
        assert!(status()["slots"]["moip"].as_array().unwrap().is_empty());
    }
}
