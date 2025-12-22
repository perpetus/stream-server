use std::collections::BTreeMap;


#[derive(Debug, Clone)]
pub enum BValue {
    Int(i64),
    Str(Vec<u8>),
    List(Vec<BValue>),
    Dict(BTreeMap<Vec<u8>, BValue>),
}

impl BValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            BValue::Str(bytes) => std::str::from_utf8(bytes).ok(),
            _ => None,
        }
    }
    
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
             BValue::Str(bytes) => Some(bytes),
             _ => None,
        }
    }

    pub fn get_dict(&self) -> Option<&BTreeMap<Vec<u8>, BValue>> {
        match self {
            BValue::Dict(map) => Some(map),
            _ => None,
        }
    }
    
    pub fn get_list(&self) -> Option<&Vec<BValue>> {
        match self {
            BValue::List(list) => Some(list),
            _ => None,
        }
    }
    
    pub fn get_int(&self) -> Option<i64> {
         match self {
             BValue::Int(i) => Some(*i),
             _ => None,
         }
    }
}

pub struct Torrent {
    pub info_hash: String,
    pub files: Vec<TorrentFile>,
    pub _name: Option<String>,
}

pub struct TorrentFile {
    pub path: String,
    pub _length: u64,
}

pub fn parse_torrent(bytes: &[u8]) -> Result<Torrent, String> {
    let (root, _) = parse_bencode(bytes).map_err(|_| "Invalid bencode")?;
    let dict = root.get_dict().ok_or("Root not a dict")?;
    
    let info = dict.get(b"info".as_slice()).ok_or("Missing info dict")?;
    
    // Calculate InfoHash
    // We need the RAW bytes of the info dict. 
    // Since our parser is simple, we might not have tracked the raw slice.
    // We need to re-encode or extract the slice. 
    // Easier: find "4:info" in original bytes and parse just that part to get the end index.
    
    let info_bytes = extract_info_bytes(bytes).ok_or("Could not extract info bytes")?;
    let info_hash = sha1_str(info_bytes);
    
    let info_dict = info.get_dict().ok_or("Info not a dict")?;
    let name = info_dict.get(b"name".as_slice()).and_then(|v| v.as_str()).map(|s| s.to_string());
    
    let mut files = Vec::new();
    
    if let Some(files_list) = info_dict.get(b"files".as_slice()).and_then(|v| v.get_list()) {
        // Multi-file
        for file_dict in files_list {
            if let Some(f_dict) = file_dict.get_dict() {
                let length = f_dict.get(b"length".as_slice()).and_then(|v| v.get_int()).unwrap_or(0) as u64;
                let path_list = f_dict.get(b"path".as_slice()).and_then(|v| v.get_list());
                if let Some(p_list) = path_list {
                    let mut path_parts = Vec::new();
                     for p in p_list {
                         if let Some(s) = p.as_str() {
                             path_parts.push(s);
                         }
                     }
                     if !path_parts.is_empty() {
                         files.push(TorrentFile {
                             path: path_parts.join("/"),
                             _length: length,
                         });
                     }
                }
            }
        }
    } else {
        // Single-file
        let length = info_dict.get(b"length".as_slice()).and_then(|v| v.get_int()).unwrap_or(0) as u64;
        if let Some(n) = &name {
             files.push(TorrentFile {
                 path: n.clone(),
                 _length: length,
             });
        }
    }
    
    Ok(Torrent {
        info_hash,
        files,
        _name: name,
    })
}

fn parse_bencode(bytes: &[u8]) -> Result<(BValue, usize), ()> {
    if bytes.is_empty() { return Err(()); }
    match bytes[0] {
        b'i' => {
            let end = bytes.iter().position(|&b| b == b'e').ok_or(())?;
            let s = std::str::from_utf8(&bytes[1..end]).map_err(|_| ())?;
            let i = s.parse::<i64>().map_err(|_| ())?;
            Ok((BValue::Int(i), end + 1))
        },
        b'l' => {
            let mut list = Vec::new();
            let mut offset = 1;
            while offset < bytes.len() && bytes[offset] != b'e' {
                let (val, len) = parse_bencode(&bytes[offset..])?;
                list.push(val);
                offset += len;
            }
            if offset >= bytes.len() { return Err(()); }
            Ok((BValue::List(list), offset + 1))
        },
        b'd' => {
            let mut map = BTreeMap::new();
            let mut offset = 1;
            while offset < bytes.len() && bytes[offset] != b'e' {
                let (key_val, k_len) = parse_bencode(&bytes[offset..])?;
                offset += k_len;
                let key = key_val.as_bytes().ok_or(())?.to_vec();
                
                let (val, v_len) = parse_bencode(&bytes[offset..])?;
                offset += v_len;
                map.insert(key, val);
            }
            if offset >= bytes.len() { return Err(()); }
            Ok((BValue::Dict(map), offset + 1))
        },
        c if c.is_ascii_digit() => {
            let colon = bytes.iter().position(|&b| b == b':').ok_or(())?;
            let len_str = std::str::from_utf8(&bytes[0..colon]).map_err(|_| ())?;
            let len = len_str.parse::<usize>().map_err(|_| ())?;
            let start = colon + 1;
            if start + len > bytes.len() { return Err(()); }
            Ok((BValue::Str(bytes[start..start+len].to_vec()), start + len))
        },
        _ => Err(()),
    }
}

// Minimal SHA1 implementation to avoid pulling dependencies
struct Sha1 {
    h: [u32; 5],
}

impl Sha1 {
    fn new() -> Self {
        Self { h: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0] }
    }

    fn update(&mut self, data: &[u8]) {
        let mut chunks = data.chunks_exact(64);
        for chunk in chunks.by_ref() {
            self.process_chunk(chunk.try_into().unwrap());
        }
        // Handle last chunk is done by caller usually, but here we do simple one-shot style or simple chunking?
        // Actually, for full SHA1 we need padding.
    }
    
    fn process_chunk(&mut self, chunk: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (i, v) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(v.try_into().unwrap());
        }
        for i in 16..80 {
            let x = w[i-3] ^ w[i-8] ^ w[i-14] ^ w[i-16];
            w[i] = x.rotate_left(1);
        }

        let mut a = self.h[0];
        let mut b = self.h[1];
        let mut c = self.h[2];
        let mut d = self.h[3];
        let mut e = self.h[4];

        for i in 0..80 {
            let (f, k) = if i < 20 {
                ((b & c) | (!b & d), 0x5A827999)
            } else if i < 40 {
                (b ^ c ^ d, 0x6ED9EBA1)
            } else if i < 60 {
                ((b & c) | (b & d) | (c & d), 0x8F1BBCDC)
            } else {
                (b ^ c ^ d, 0xCA62C1D6)
            };

            let temp = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        self.h[0] = self.h[0].wrapping_add(a);
        self.h[1] = self.h[1].wrapping_add(b);
        self.h[2] = self.h[2].wrapping_add(c);
        self.h[3] = self.h[3].wrapping_add(d);
        self.h[4] = self.h[4].wrapping_add(e);
    }
}

pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut sha1 = Sha1::new();
    // Padding
    let mut padded = Vec::with_capacity(data.len() + 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() + 8) % 64 != 0 {
        padded.push(0);
    }
    let len_bits = (data.len() as u64) * 8;
    padded.extend_from_slice(&len_bits.to_be_bytes());
    
    sha1.update(&padded);
    
    let mut res = [0u8; 20];
    for (i, val) in sha1.h.iter().enumerate() {
        let bytes = val.to_be_bytes();
        res[i*4..(i+1)*4].copy_from_slice(&bytes);
    }
    res
}

fn sha1_str(data: &[u8]) -> String {
    let digest = sha1_digest(data);
    hex::encode(digest)
}

fn extract_info_bytes(bytes: &[u8]) -> Option<&[u8]> {
    // Find "4:info"
    let pattern = b"4:info";
    let start = bytes.windows(pattern.len()).position(|w| w == pattern)? + pattern.len();
    
    // Now decode the bencode object starting at 'start' to find its length
    let (_, len) = parse_bencode(&bytes[start..]).ok()?;
    Some(&bytes[start..start+len])
}
