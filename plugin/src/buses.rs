// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! The TAP's virtual buses (PipeWire / PulseAudio null sinks): shared vocabulary between the
//! patchbay (which makes them) and the hardware grid (which sends HW 1 / HW 2 to the cards).

/// The TAP's two outputs to hardware: the hardware grid sends each to any sound cards.
pub const HW1: &str = "wbs_hw1";
pub const HW2: &str = "wbs_hw2";

pub const WBS_SINKS: [(&str, &str, &str); 6] = [
    ("wbs_input_bus", "Ward-Beck TAP 1: Input Bus (default output, every app plays here)", "TAP Input Bus"),
    ("wbs_insert_bus", "Ward-Beck TAP 2: Insert Bus (selected source, feeds the speakers)", "TAP Insert Bus"),
    ("wbs_meter_tap", "Ward-Beck TAP 3: Meter Tap (selected source, feeds the XTM meters)", "TAP Meter Tap"),
    ("wbs_test_point", "Ward-Beck TAP 4: Test Point (post-insert copy for analysers)", "TAP Test Point"),
    ("wbs_hw1", "Ward-Beck TAP 5: HW 1 (Speakers)", "TAP HW 1 Speakers"),
    ("wbs_hw2", "Ward-Beck TAP 6: HW 2 (Cue/HP)", "TAP HW 2 Cue/HP"),
];
