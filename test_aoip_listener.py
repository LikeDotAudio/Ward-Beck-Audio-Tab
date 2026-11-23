# Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
"""Unit tests for aoip_listener packet parsers (hand-built packets, no network)."""

import socket
import struct
import time
import unittest

import aoip_listener as al

GM = bytes.fromhex("001DC1FFFE123456")
GM_STR = "00-1D-C1-FF-FE-12-34-56"

SDP = (
    "v=0\r\n"
    "o=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n"
    "s=Stage Box 1-2\r\n"
    "c=IN IP4 239.69.1.10/32\r\n"
    "t=0 0\r\n"
    "a=ts-refclk:ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0\r\n"
    "a=mediaclk:direct=0\r\n"
    "m=audio 5004 RTP/AVP 96\r\n"
    "a=rtpmap:96 L24/48000/2\r\n"
    "a=ptime:1\r\n"
)


def sap_packet(sdp, deletion=False, msg_hash=0xBEEF, origin="192.168.1.50", ptype=True):
    b0 = (1 << 5) | (0x04 if deletion else 0)
    hdr = bytes([b0, 0]) + struct.pack("!H", msg_hash) + socket.inet_aton(origin)
    body = (b"application/sdp\x00" if ptype else b"") + sdp.encode()
    return hdr + body


def ptp2_announce(clock=GM, gm=GM, domain=0):
    hdr = bytearray(34)
    hdr[0] = 0x0B
    hdr[1] = 0x02
    struct.pack_into("!H", hdr, 2, 64)
    hdr[4] = domain
    hdr[20:28] = clock
    struct.pack_into("!H", hdr, 28, 1)
    struct.pack_into("!H", hdr, 30, 42)
    hdr[32] = 5
    hdr[33] = 1
    body = bytearray(30)
    struct.pack_into("!h", body, 10, 37)      # currentUtcOffset (offset 44)
    body[13] = 128                            # priority1 (47)
    body[14] = 248                            # clockClass (48)
    body[15] = 0x20                           # clockAccuracy (49)
    struct.pack_into("!H", body, 16, 0x4E5D)  # offsetScaledLogVariance (50)
    body[18] = 127                            # priority2 (52)
    body[19:27] = gm                          # grandmasterIdentity (53)
    struct.pack_into("!H", body, 27, 0)       # stepsRemoved (61)
    body[29] = 0xA0                           # timeSource (63)
    return bytes(hdr + body)


def ptp1_sync(uuid=bytes.fromhex("001DC1123456"), gm=bytes.fromhex("001DC1123456")):
    p = bytearray(124)
    struct.pack_into("!H", p, 0, 1)
    struct.pack_into("!H", p, 2, 1)
    p[4:9] = b"_DFLT"
    p[20] = 1          # event message
    p[21] = 1          # ethernet
    p[22:28] = uuid
    struct.pack_into("!H", p, 28, 1)
    struct.pack_into("!H", p, 30, 7)
    p[32] = 0          # control = Sync
    p[53] = 1
    p[54:60] = gm
    struct.pack_into("!H", p, 60, 1)
    p[67] = 4          # stratum
    p[68:72] = b"DFLT"
    p[77] = 1          # preferred
    struct.pack_into("!H", p, 90, 0)
    return bytes(p)


SIP_REGISTER = (
    "REGISTER sip:sip.mcast.net SIP/2.0\r\n"
    "Via: SIP/2.0/UDP 192.168.1.60:5060\r\n"
    "From: <sip:intercom@192.168.1.60>;tag=1234\r\n"
    "To: <sip:intercom@192.168.1.60>\r\n"
    "Call-ID: abc123@192.168.1.60\r\n"
    "CSeq: 1 REGISTER\r\n"
    "Contact: <sip:intercom@192.168.1.60:5060>\r\n"
    "User-Agent: WBS-Intercom/1.0\r\n"
    "Content-Length: 0\r\n\r\n"
).encode()


class ParserTests(unittest.TestCase):
    # These exercise the on-the-wire decoders, so pin the PTP source to sockets mode.
    def tearDown(self):
        al.PTP_MODE = self._mode

    def setUp(self):
        self._mode, al.PTP_MODE = al.PTP_MODE, "sockets"
        with al._LOCK:
            al._SAP_SESSIONS.clear()
            al._PTP_CLOCKS.clear()
            al._SIP_MESSAGES.clear()

    def test_sap_announcement(self):
        pkt = al.parse_sap(sap_packet(SDP))
        self.assertEqual(pkt["version"], 1)
        self.assertFalse(pkt["deletion"])
        self.assertEqual(pkt["msg_id_hash"], 0xBEEF)
        self.assertEqual(pkt["origin_ip"], "192.168.1.50")
        self.assertEqual(pkt["payload_type"], "application/sdp")
        s = pkt["sdp"]
        self.assertEqual(s["session_name"], "Stage Box 1-2")
        self.assertEqual(s["multicast_ip"], "239.69.1.10")
        self.assertEqual(s["ttl"], 32)
        self.assertEqual(s["port"], 5004)
        self.assertEqual((s["encoding"], s["sample_rate"], s["channels"]), ("L24", 48000, 2))
        self.assertEqual(s["ptime"], 1.0)
        self.assertEqual(s["ptp_grandmaster"], GM_STR)
        self.assertEqual(s["ptp_domain"], 0)
        self.assertEqual(s["mediaclk"], "direct=0")
        # without the optional payload-type string
        self.assertEqual(al.parse_sap(sap_packet(SDP, ptype=False))["sdp"]["port"], 5004)

    def test_sap_deletion_drops_session(self):
        al._handle_sap(sap_packet(SDP), ("192.168.1.50", 9875), time.time())
        snap = al.snapshot()
        self.assertEqual(len(snap["sap"]["sessions"]), 1)
        sess = snap["sap"]["sessions"][0]
        for k in ("id", "origin_ip", "session_name", "multicast_ip", "port", "encoding",
                  "sample_rate", "channels", "ptime", "ptp_grandmaster", "ptp_domain",
                  "last_seen", "age_s"):
            self.assertIn(k, sess)
        dele = al.parse_sap(sap_packet("v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n", deletion=True))
        self.assertTrue(dele["deletion"])
        al._handle_sap(sap_packet("v=0\r\no=- 1311738121 1311738121 IN IP4 192.168.1.50\r\n", deletion=True),
                       ("192.168.1.50", 9875), time.time())
        self.assertEqual(al.snapshot()["sap"]["sessions"], [])

    def test_sap_expiry(self):
        al._handle_sap(sap_packet(SDP), ("192.168.1.50", 9875), 1.0)  # ancient
        self.assertEqual(al.snapshot()["sap"]["sessions"], [])

    def test_ptp2_announce(self):
        m = al.parse_ptp(ptp2_announce(domain=3))
        self.assertEqual(m["version"], 2)
        self.assertEqual(m["message_name"], "Announce")
        self.assertEqual(m["domain"], 3)
        self.assertEqual(m["clock_identity"], GM_STR)
        self.assertEqual(m["sequence_id"], 42)
        a = m["announce"]
        self.assertEqual(a["grandmaster_identity"], GM_STR)
        self.assertEqual((a["priority1"], a["priority2"]), (128, 127))
        self.assertEqual(a["clock_class"], 248)
        self.assertEqual(a["clock_accuracy"], "0x20")
        self.assertEqual(a["offset_scaled_log_variance"], "0x4E5D")
        self.assertEqual(a["steps_removed"], 0)
        self.assertEqual(a["time_source"], "INTERNAL_OSCILLATOR")

        import time
        al._handle_ptp(ptp2_announce(), ("192.168.1.70", 320), time.time())
        snap = al.snapshot()["ptp"]
        c = snap["clocks"][0]
        self.assertEqual(c["network"], "ravenna/aes67")
        self.assertTrue(c["is_grandmaster"])
        self.assertEqual(c["announce_count"], 1)
        self.assertEqual(snap["grandmasters"][0]["identity"], GM_STR)
        self.assertEqual(snap["grandmasters"][0]["source_ip"], "192.168.1.70")
        al.set_dante_ip_provider(lambda: ["192.168.1.70"])
        try:
            self.assertEqual(al.snapshot()["ptp"]["clocks"][0]["network"], "dante (aes67 mode)")
        finally:
            al.set_dante_ip_provider(None)

    def test_ptp1_sync(self):
        m = al.parse_ptp(ptp1_sync())
        self.assertEqual(m["version"], 1)
        self.assertEqual(m["subdomain"], "_DFLT")
        self.assertEqual(m["message_name"], "Sync")
        self.assertEqual(m["message_type"], 1)
        self.assertEqual(m["clock_identity"], "00-1D-C1-12-34-56")
        self.assertEqual(m["sequence_id"], 7)
        s = m["sync"]
        self.assertEqual(s["grandmaster_identity"], "00-1D-C1-12-34-56")
        self.assertEqual(s["grandmaster_stratum"], 4)
        self.assertEqual(s["grandmaster_identifier"], "DFLT")
        self.assertTrue(s["grandmaster_preferred"])

        import time
        al._handle_ptp(ptp1_sync(), ("192.168.1.80", 319), time.time())
        snap = al.snapshot()["ptp"]
        c = snap["clocks"][0]
        self.assertEqual(c["network"], "dante")
        self.assertEqual(c["domain"], "_DFLT")
        self.assertEqual(c["sync_count"], 1)
        self.assertTrue(c["is_grandmaster"])
        self.assertEqual(snap["grandmasters"][0]["network"], "dante")

    def test_ptp_rejects_garbage(self):
        with self.assertRaises(ValueError):
            al.parse_ptp(b"\x00" * 10)

    def test_sip_register(self):
        m = al.parse_sip(SIP_REGISTER)
        self.assertEqual(m["method"], "REGISTER")
        self.assertTrue(m["is_request"])
        self.assertEqual(m["from"], "<sip:intercom@192.168.1.60>;tag=1234")
        self.assertEqual(m["to"], "<sip:intercom@192.168.1.60>")
        self.assertEqual(m["call_id"], "abc123@192.168.1.60")
        self.assertEqual(m["contact"], "<sip:intercom@192.168.1.60:5060>")
        self.assertEqual(m["user_agent"], "WBS-Intercom/1.0")
        import time
        al._handle_sip(SIP_REGISTER, ("192.168.1.60", 5060), time.time())
        al._handle_sip(SIP_REGISTER, ("192.168.1.60", 5060), time.time())
        msgs = al.snapshot()["sip"]["messages"]
        self.assertEqual(len(msgs), 1)
        self.assertEqual(msgs[0]["count"], 2)
        self.assertEqual(msgs[0]["source_ip"], "192.168.1.60")

    def test_snapshot_contract(self):
        snap = al.snapshot()
        self.assertEqual(set(snap), {"iface", "sap", "ptp", "sip"})
        for k in ("sap", "ptp", "sip"):
            for f in ("listening", "error", "groups", "packets"):
                self.assertIn(f, snap[k])
        self.assertEqual(snap["sap"]["groups"], ["239.255.255.255:9875", "224.2.127.254:9875"])
        self.assertEqual(snap["ptp"]["groups"], ["224.0.1.129:319", "224.0.1.129:320"])
        self.assertEqual(snap["sip"]["groups"], ["224.0.1.75:5060"])



class TestPtpDiscovery(unittest.TestCase):
    """Passive PTP: rows from the apk-ptp retained MQTT topic."""

    ROWS = [
        {"clock_id": "00:07:F5:FF:FE:42:06:A2", "variant": "PTPv2/UDP", "vendor": "Bridgeco Co AG",
         "domain": "44", "subdomain": "-", "role": "grandmaster", "grandmaster": "00:07:F5:FF:FE:42:06:A2",
         "priority1": "200", "priority2": "200", "gm_class": "255", "gm_accuracy": "100ns",
         "steps_removed": "0", "sync_health": "contested", "_row_state": "online",
         "messages": "Sync \u00d711829, Follow_Up \u00d710062, Announce \u00d74981"},
        {"clock_id": "00:0A:92:FF:FE:02:77:5B", "variant": "PTPv1", "vendor": "Presonus Corporation",
         "domain": "0", "subdomain": "_DFLT", "role": "clock", "grandmaster": "-", "priority1": "-",
         "_row_state": "online", "messages": "Sync \u00d745950, Follow_Up \u00d739562"},
    ]

    def test_mapping(self):
        a, b = al.parse_ptp_discovery(self.ROWS)
        self.assertEqual(a["clock_identity"], "00-07-F5-FF-FE-42-06-A2")
        self.assertEqual((a["version"], a["domain"], a["network"]), (2, 44, "ravenna/aes67"))
        self.assertTrue(a["is_grandmaster"])
        self.assertEqual((a["priority1"], a["sync_count"], a["announce_count"]), (200, 11829, 4981))
        self.assertEqual((b["version"], b["domain"], b["network"]), (1, "_DFLT", "dante"))
        self.assertIsNone(b["grandmaster_identity"])
        self.assertEqual(b["sync_count"], 45950)

    def test_garbage(self):
        self.assertEqual(al.parse_ptp_discovery({"not": "a list"}), [])

if __name__ == "__main__":
    unittest.main()
