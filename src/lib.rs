// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! WARD BECK TAP: virtual audio patchbay, non-destructive signal tap, MXL (EBU DMF)
//! shared-memory engine, test points and AoIP listeners for Linux.
//!
//! Module map (each was ported from the Python file named alongside):
//!   util      shared helpers (commands, shutdown flag, PCM chunk, dB)
//!   mxl       mxl_engine.py          MXL domain/flow rings in /dev/shm/mxl
//!   testpoint test_point.py          one test point at a time, own ring + meters
//!   aoip      aoip_listener.py       SAP/SDP, PTP, SIP multicast + PTP over MQTT
//!   aes67     (new)                  AES67 RTP + SAP transmitter per MXL flow
//!   pipeline  pipeline_inspector.py  ALSA / PipeWire / MXL / AoIP layer report
//!   listen    listen_picker.py       "what should the meters hear?" picker
//!   setup     setup_page.py          /setup deployment guide
//!   tap       ward-beck-audio-tab.py routing, sources, meters, generators, reset
//!   cbc       ward-beck-audio-tab.py CBC / Radio-Canada MXL test-set proxy
//!   nmos      ward-beck-audio-tab.py NMOS IS-04 / AoIP discovery
//!   http      ward-beck-audio-tab.py HTTP API + UI on :8467

pub mod util;
pub mod mxl;
pub mod testpoint;
pub mod aoip;
pub mod ntp;
pub mod aes67;
pub mod pipeline;
pub mod listen;
pub mod setup;
pub mod tap;
pub mod cbc;
pub mod nmos;
pub mod http;
pub mod hwout;
