// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The TAP's virtual buses (PipeWire / PulseAudio null sinks): shared vocabulary between the
//! patchbay (which makes them) and the hardware grid (which sends HW 1 / HW 2 to the cards).

/// The TAP's two outputs to hardware: the hardware grid sends each to any sound cards.
pub const HW1: &str = "wbs_hw1";
pub const HW2: &str = "wbs_hw2";

/// Made in this order, so the desktop's sound menu reads as the signal path: apps play
/// "To Tap", the TAP sends HW 1 / HW 2 to the sound cards; the internal buses come last.
pub const WBS_SINKS: [(&str, &str, &str); 6] = [
    ("wbs_input_bus", "Ward-Beck TAP: To Tap (apps play here)", "To Tap"),
    ("wbs_hw1", "Ward-Beck TAP: HW 1 (Speakers)", "TAP HW 1 Speakers"),
    ("wbs_hw2", "Ward-Beck TAP: HW 2 (Cue/HP)", "TAP HW 2 Cue/HP"),
    ("wbs_insert_bus", "Ward-Beck TAP: internal Insert (not for apps)", "TAP internal Insert"),
    ("wbs_meter_tap", "Ward-Beck TAP: internal Meter Tap (not for apps)", "TAP internal Meter Tap"),
    ("wbs_test_point", "Ward-Beck TAP: internal Test Point (not for apps)", "TAP internal Test Point"),
];
