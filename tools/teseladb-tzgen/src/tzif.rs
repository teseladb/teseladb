//! Reading the TZif files `zic` writes (RFC 8536): the 64-bit data of version 2 and later and
//! its footer.

/// One local time type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    /// Seconds east of UTC.
    pub offset: i32,
    /// Daylight saving time.
    pub dst: bool,
    /// Abbreviation.
    pub abbr: String,
}

/// A zone's data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tzif {
    /// Transition instants.
    pub transitions: Vec<i64>,
    /// The type of each transition.
    pub indices: Vec<u8>,
    /// The types.
    pub types: Vec<Type>,
    /// The POSIX rule after the last transition.
    pub footer: String,
}

const MAGIC: &[u8; 4] = b"TZif";
const HEADER: usize = 44;

struct Counts {
    isut: usize,
    isstd: usize,
    leap: usize,
    time: usize,
    types: usize,
    chars: usize,
}

fn be32(b: &[u8], at: usize) -> Result<u32, String> {
    let s = b.get(at..at + 4).ok_or("truncated")?;
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn counts(b: &[u8], at: usize) -> Result<Counts, String> {
    if b.get(at..at + 4) != Some(MAGIC.as_slice()) {
        return Err("not a TZif header".to_owned());
    }
    let n = |i: usize| -> Result<usize, String> { Ok(be32(b, at + 20 + 4 * i)? as usize) };
    Ok(Counts { isut: n(0)?, isstd: n(1)?, leap: n(2)?, time: n(3)?, types: n(4)?, chars: n(5)? })
}

impl Counts {
    fn data_len(&self, time_size: usize) -> usize {
        self.time * time_size
            + self.time
            + self.types * 6
            + self.chars
            + self.leap * (time_size + 4)
            + self.isstd
            + self.isut
    }
}

impl Tzif {
    /// The data of a TZif file; `None` for a file that is not one.
    pub fn parse(b: &[u8]) -> Result<Option<Tzif>, String> {
        if b.get(..4) != Some(MAGIC.as_slice()) {
            return Ok(None);
        }
        let v1 = counts(b, 0)?;
        let second = HEADER + v1.data_len(4);
        let c = counts(b, second)?;
        let mut at = second + HEADER;
        let mut transitions = Vec::with_capacity(c.time);
        for i in 0..c.time {
            let s = b.get(at + 8 * i..at + 8 * i + 8).ok_or("truncated transitions")?;
            transitions.push(i64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]));
        }
        at += 8 * c.time;
        let indices = b.get(at..at + c.time).ok_or("truncated indices")?.to_vec();
        at += c.time;
        let infos = b.get(at..at + 6 * c.types).ok_or("truncated types")?.to_vec();
        at += 6 * c.types;
        let chars = b.get(at..at + c.chars).ok_or("truncated abbreviations")?;
        at += c.chars + c.leap * 12 + c.isstd + c.isut;
        let mut types = Vec::with_capacity(c.types);
        for t in infos.chunks(6) {
            let offset = i32::from_be_bytes([t[0], t[1], t[2], t[3]]);
            let start = usize::from(t[5]);
            let end =
                chars[start..].iter().position(|x| *x == 0).map_or(chars.len(), |p| start + p);
            let abbr = String::from_utf8_lossy(&chars[start..end]).into_owned();
            types.push(Type { offset, dst: t[4] != 0, abbr });
        }
        let footer = b
            .get(at..)
            .map(|f| String::from_utf8_lossy(f).trim_matches('\n').to_owned())
            .unwrap_or_default();
        Ok(Some(Tzif { transitions, indices, types, footer }))
    }
}
