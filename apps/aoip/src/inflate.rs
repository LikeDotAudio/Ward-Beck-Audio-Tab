// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Minimal zlib (RFC 1950) / DEFLATE (RFC 1951) decoder for compressed SAP payloads
//! (Python: zlib.decompress). Stored, fixed and dynamic Huffman blocks; Adler-32 checked.

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    bit: u32,
    nbits: u32,
}

impl<'a> Bits<'a> {
    fn need(&mut self, n: u32) -> Result<(), String> {
        while self.nbits < n {
            let b = *self.d.get(self.pos).ok_or("incomplete or truncated stream")?;
            self.pos += 1;
            self.bit |= (b as u32) << self.nbits;
            self.nbits += 8;
        }
        Ok(())
    }
    fn get(&mut self, n: u32) -> Result<u32, String> {
        if n == 0 {
            return Ok(0);
        }
        self.need(n)?;
        let v = self.bit & ((1u32 << n) - 1);
        self.bit >>= n;
        self.nbits -= n;
        Ok(v)
    }
    fn align(&mut self) {
        self.bit = 0;
        self.nbits = 0;
    }
}

struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Huff {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Huff { counts, symbols }
    }
    fn decode(&self, b: &mut Bits) -> Result<u16, String> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.get(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return self
                    .symbols
                    .get((index + (code - first)) as usize)
                    .copied()
                    .ok_or_else(|| "invalid code".to_string());
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err("invalid code".into())
    }
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn codes(b: &mut Bits, out: &mut Vec<u8>, lit: &Huff, dist: &Huff) -> Result<(), String> {
    loop {
        let sym = lit.decode(b)? as usize;
        if sym < 256 {
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let s = sym - 257;
            if s >= 29 {
                return Err("invalid literal/length code".into());
            }
            let len = LBASE[s] as usize + b.get(LEXT[s] as u32)? as usize;
            let ds = dist.decode(b)? as usize;
            if ds >= 30 {
                return Err("invalid distance code".into());
            }
            let d = DBASE[ds] as usize + b.get(DEXT[ds] as u32)? as usize;
            if d > out.len() {
                return Err("invalid distance too far back".into());
            }
            let start = out.len() - d;
            for i in 0..len {
                let c = out[start + i];
                out.push(c);
            }
        }
    }
}

/// Raw DEFLATE stream -> (bytes, bytes consumed).
pub fn inflate_raw(data: &[u8]) -> Result<(Vec<u8>, usize), String> {
    let mut b = Bits { d: data, pos: 0, bit: 0, nbits: 0 };
    let mut out = Vec::new();
    loop {
        let last = b.get(1)?;
        match b.get(2)? {
            0 => {
                b.align();
                let p = b.pos;
                if p + 4 > data.len() {
                    return Err("incomplete or truncated stream".into());
                }
                let len = u16::from_le_bytes([data[p], data[p + 1]]) as usize;
                let nlen = u16::from_le_bytes([data[p + 2], data[p + 3]]) as usize;
                if len != (!nlen & 0xFFFF) {
                    return Err("invalid stored block lengths".into());
                }
                if p + 4 + len > data.len() {
                    return Err("incomplete or truncated stream".into());
                }
                out.extend_from_slice(&data[p + 4..p + 4 + len]);
                b.pos = p + 4 + len;
            }
            1 => {
                let mut l = [0u8; 288];
                for (i, v) in l.iter_mut().enumerate() {
                    *v = match i {
                        0..=143 => 8,
                        144..=255 => 9,
                        256..=279 => 7,
                        _ => 8,
                    };
                }
                let lit = Huff::new(&l);
                let dist = Huff::new(&[5u8; 30]);
                codes(&mut b, &mut out, &lit, &dist)?;
            }
            2 => {
                let hlit = b.get(5)? as usize + 257;
                let hdist = b.get(5)? as usize + 1;
                let hclen = b.get(4)? as usize + 4;
                const ORD: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                let mut cl = [0u8; 19];
                for &o in ORD.iter().take(hclen) {
                    cl[o] = b.get(3)? as u8;
                }
                let clh = Huff::new(&cl);
                let mut lens = vec![0u8; hlit + hdist];
                let mut i = 0;
                while i < hlit + hdist {
                    let sym = clh.decode(&mut b)?;
                    let (val, rep) = match sym {
                        0..=15 => (sym as u8, 1),
                        16 => {
                            if i == 0 {
                                return Err("invalid bit length repeat".into());
                            }
                            (lens[i - 1], 3 + b.get(2)? as usize)
                        }
                        17 => (0, 3 + b.get(3)? as usize),
                        _ => (0, 11 + b.get(7)? as usize),
                    };
                    if i + rep > hlit + hdist {
                        return Err("invalid bit length repeat".into());
                    }
                    for _ in 0..rep {
                        lens[i] = val;
                        i += 1;
                    }
                }
                let lit = Huff::new(&lens[..hlit]);
                let dist = Huff::new(&lens[hlit..]);
                codes(&mut b, &mut out, &lit, &dist)?;
            }
            _ => return Err("invalid block type".into()),
        }
        if last == 1 {
            break;
        }
    }
    Ok((out, b.pos))
}

fn adler32(d: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in d {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Python zlib.decompress(data) (zlib wrapper).
pub fn zlib_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.len() < 2 {
        return Err("incomplete or truncated stream".into());
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0F != 8 || ((cmf as u16) << 8 | flg as u16) % 31 != 0 {
        return Err("incorrect header check".into());
    }
    if flg & 0x20 != 0 {
        return Err("need dictionary".into());
    }
    let (out, used) = inflate_raw(&data[2..])?;
    let p = 2 + used;
    if p + 4 > data.len() {
        return Err("incomplete or truncated stream".into());
    }
    let want = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
    if want != adler32(&out) {
        return Err("incorrect data check".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn sample1() -> Vec<u8> {
        let mut d = b"v=0\r\ns=hello hello hello hello\r\n".repeat(20);
        d.extend(0u8..=255);
        d
    }

    fn sample2() -> Vec<u8> {
        (0..60)
            .map(|i| format!("a=x-line:{} some text here {}", i, "abcdefghij".chars().nth(i % 10).unwrap().to_string().repeat(i % 7)))
            .collect::<Vec<_>>()
            .join("\r\n")
            .into_bytes()
    }

    // Generated with Python zlib.compress at levels 0 (stored), 1 (fixed) and 9 (dynamic).
    const Z0: &str = "78010180037ffc763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a763d300d0a733d68656c6c6f2068656c6c6f2068656c6c6f2068656c6c6f0d0a000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeffe58c5064";
    const Z1: &str = "78012bb335e0e52ab6cd48cdc9c957c02079b9ca46e547c38786e9838191899985958d9d83938b9b87978f5f4050485844544c5c42524a5a46564e5e4151495945554d5d43534b5b47574fdfc0d0c8d8c4d4ccdcc2d2cadac6d6cedec1d1c9d9c5d5cdddc3d3cbdbc7d7cf3f2030283824342c3c22322a3a26362e3e2131293925352d3d23332b3b27372fbfa0b0a8b8a4b4acbca2b2aabaa6b6aebea1b1a9b9a5b5adbda3b3abbba7b7af7fc2c44993a74c9d367dc6cc59b3e7cc9d377fc1c2458b972c5db67cc5ca55abd7ac5db77ec3c64d9bb76cddb67dc7ce5dbbf7ecddb7ffc0c143878f1c3d76fcc4c953a7cf9c3d77fec2c54b97af5cbd76fdc6cd5bb7efdcbd77ffc1c3478f9f3c7df6fcc5cb57afdfbc7df7fec3c74f9fbf7cfdf6fdc7cf5fbffffcfdf71f00e58c5064";
    const Z2: &str = "78da7d95cb0ac2301045f782ffd01f106c9afa023f26b6d15a7c80baf0f3ad827092b964b6e76ee6f44e1af6efc5e57c8bbb65f5bc5f63f58aef5735c447ace6b3f06775ce0e802e875d07dae4b4ef7b609fe3380d789bf3e3771058e581d36f90581716dbe4ec0cb8cde138d28911164220b7d2a6614088eb1277b59297faab7de9b3597ba4421db1f1360c03b975370d03c2df9828744a616ad1d585fd9c35482aec11abea91cbea31a0abc78452985a749bd27ed6202f4bd82356fd2397f56340b78f0979bd89c5a62decd75883a4c21eb1ea1fb9ac1f9f1edd3e26e4092716bd2bece7ad4152618f58f58f5cd68f01dd3e26e4092716fdb6b05f6b0d920a7bc4aa7fe4b27e0ce8f631a1ff1eb4d8ae4afb5983a4c21eb1eadf07e4ef6e1c";

    #[test]
    fn stored_fixed_dynamic() {
        assert_eq!(zlib_decompress(&hex(Z0)).unwrap(), sample1());
        assert_eq!(zlib_decompress(&hex(Z1)).unwrap(), sample1());
        assert_eq!(zlib_decompress(&hex(Z2)).unwrap(), sample2());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(zlib_decompress(b"\x00\x00garbage").is_err());
        let mut z = hex(Z1);
        let n = z.len();
        z[n - 1] ^= 1;
        assert!(zlib_decompress(&z).is_err());
    }
}
