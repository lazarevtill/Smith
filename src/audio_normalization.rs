//! Shared, allocation-bounded admission for selected local audio. No decoding,
//! filesystem access, URLs, or external references. Unknown shapes fail closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioError {
    Unsupported,
    Limit,
}
type Result<T> = std::result::Result<T, AudioError>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Format {
    Wav,
    Aiff,
    Aac,
    Alac,
}
impl Format {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Wav => "pcm_wav",
            Self::Aiff => "pcm_aiff",
            Self::Aac => "aac_m4a",
            Self::Alac => "alac_m4a",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Audio {
    pub(crate) format: Format,
    pub(crate) seconds: f64,
    pub(crate) channels: u16,
}
const MAX: usize = 8 * 1024 * 1024;
fn bad<T>() -> Result<T> {
    Err(AudioError::Unsupported)
}
fn u16be(b: &[u8], n: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        b.get(n..n + 2)
            .ok_or(AudioError::Unsupported)?
            .try_into()
            .unwrap(),
    ))
}
fn u32be(b: &[u8], n: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        b.get(n..n + 4)
            .ok_or(AudioError::Unsupported)?
            .try_into()
            .unwrap(),
    ))
}
fn u16le(b: &[u8], n: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(n..n + 2)
            .ok_or(AudioError::Unsupported)?
            .try_into()
            .unwrap(),
    ))
}
fn u32le(b: &[u8], n: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(n..n + 4)
            .ok_or(AudioError::Unsupported)?
            .try_into()
            .unwrap(),
    ))
}
fn duration(frames: u64, rate: u32) -> Result<f64> {
    if !(8000..=192000).contains(&rate) || frames == 0 {
        return bad();
    }
    if frames > 60 * u64::from(rate) {
        return Err(AudioError::Limit);
    }
    Ok(frames as f64 / f64::from(rate))
}
pub(crate) fn inspect(name: &str, b: &[u8]) -> Result<Audio> {
    if b.len() > MAX {
        return Err(AudioError::Limit);
    }
    if b.len() < 12 {
        return bad();
    }
    let name = name.to_ascii_lowercase();
    if name.ends_with(".wav") && &b[..4] == b"RIFF" && &b[8..12] == b"WAVE" {
        wav(b)
    } else if (name.ends_with(".aiff") || name.ends_with(".aif"))
        && &b[..4] == b"FORM"
        && &b[8..12] == b"AIFF"
    {
        aiff(b)
    } else if name.ends_with(".m4a") {
        m4a(b)
    } else {
        bad()
    }
}
fn wav(b: &[u8]) -> Result<Audio> {
    if u32le(b, 4)? as usize + 8 != b.len() {
        return bad();
    }
    let mut at = 12;
    let mut fmt = None;
    let mut data = None;
    let mut chunks = 0;
    while at < b.len() {
        chunks += 1;
        if chunks > 64 {
            return Err(AudioError::Limit);
        }
        let h = b.get(at..at + 8).ok_or(AudioError::Unsupported)?;
        let len = u32le(h, 4)? as usize;
        at += 8;
        let p = b
            .get(at..at.checked_add(len).ok_or(AudioError::Unsupported)?)
            .ok_or(AudioError::Unsupported)?;
        match &h[..4] {
            b"fmt " => {
                if fmt.is_some() || len != 16 || u16le(p, 0)? != 1 {
                    return bad();
                }
                let channels = u16le(p, 2)?;
                let rate = u32le(p, 4)?;
                let bits = u16le(p, 14)?;
                let align = u16le(p, 12)?;
                if !(1..=2).contains(&channels)
                    || !matches!(bits, 8 | 16 | 24 | 32)
                    || align != channels * (bits / 8)
                    || u32le(p, 8)?
                        != rate
                            .checked_mul(u32::from(align))
                            .ok_or(AudioError::Unsupported)?
                {
                    return bad();
                }
                fmt = Some((rate, align, channels));
            }
            b"data" => {
                if data.replace(len).is_some() {
                    return bad();
                }
            }
            b"FLLR" | b"JUNK" | b"PAD " => {
                if len > 65536 {
                    return Err(AudioError::Limit);
                }
            }
            _ => return bad(),
        }
        at = at
            .checked_add(len + len % 2)
            .ok_or(AudioError::Unsupported)?;
    }
    let (rate, align, channels) = fmt.ok_or(AudioError::Unsupported)?;
    let data = data.ok_or(AudioError::Unsupported)?;
    if at != b.len() || data % usize::from(align) != 0 {
        return bad();
    }
    Ok(Audio {
        format: Format::Wav,
        channels,
        seconds: duration((data / usize::from(align)) as u64, rate)?,
    })
}
fn aiff(b: &[u8]) -> Result<Audio> {
    if u32be(b, 4)? as usize + 8 != b.len() {
        return bad();
    }
    let mut at = 12;
    let mut fmt = None;
    let mut data = None;
    let mut chunks = 0;
    while at < b.len() {
        chunks += 1;
        if chunks > 64 {
            return Err(AudioError::Limit);
        }
        let h = b.get(at..at + 8).ok_or(AudioError::Unsupported)?;
        let len = u32be(h, 4)? as usize;
        at += 8;
        let p = b
            .get(at..at.checked_add(len).ok_or(AudioError::Unsupported)?)
            .ok_or(AudioError::Unsupported)?;
        match &h[..4] {
            b"COMM" => {
                if fmt.is_some() || len != 18 {
                    return bad();
                }
                let channels = u16be(p, 0)?;
                let frames = u32be(p, 2)?;
                let bits = u16be(p, 6)?;
                let exponent = u16be(p, 8)?;
                let mantissa = u64::from_be_bytes(p[10..18].try_into().unwrap());
                // Positive, finite, integral 80-bit sample rates only.
                if !(16395..=16400).contains(&exponent) || mantissa >> 63 != 1 {
                    return bad();
                }
                let shift = 63 - (exponent - 16383);
                let mask = (1u64 << shift) - 1;
                if mantissa & mask != 0 {
                    return bad();
                }
                let rate = (mantissa >> shift) as u32;
                if !(1..=2).contains(&channels) || !matches!(bits, 8 | 16 | 24 | 32) {
                    return bad();
                }
                fmt = Some((frames, rate, usize::from(channels * (bits / 8)), channels));
            }
            b"SSND" => {
                if data.is_some() || len < 8 {
                    return bad();
                }
                let offset = u32be(p, 0)? as usize;
                if u32be(p, 4)? != 0 || offset > 4096 || 8 + offset > len {
                    return bad();
                }
                data = Some(len - 8 - offset);
            }
            b"FLLR" | b"JUNK" => {
                if len > 65536 {
                    return Err(AudioError::Limit);
                }
            }
            _ => return bad(),
        }
        at = at
            .checked_add(len + len % 2)
            .ok_or(AudioError::Unsupported)?;
    }
    let (frames, rate, align, channels) = fmt.ok_or(AudioError::Unsupported)?;
    if at != b.len() || data != (frames as usize).checked_mul(align) {
        return bad();
    }
    Ok(Audio {
        format: Format::Aiff,
        channels,
        seconds: duration(u64::from(frames), rate)?,
    })
}
#[derive(Clone, Copy)]
struct Atom<'a> {
    tag: &'a [u8],
    p: &'a [u8],
    start: usize,
}
fn atoms(b: &[u8], base: usize) -> Result<Vec<Atom<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        if out.len() >= 128 {
            return Err(AudioError::Limit);
        }
        let n = u32be(b, at)? as usize;
        if n < 8 {
            return bad();
        }
        let end = at.checked_add(n).ok_or(AudioError::Unsupported)?;
        let p = b.get(at + 8..end).ok_or(AudioError::Unsupported)?;
        out.push(Atom {
            tag: &b[at + 4..at + 8],
            p,
            start: base + at + 8,
        });
        at = end;
    }
    Ok(out)
}
fn one<'a>(a: &[Atom<'a>], tag: &[u8]) -> Result<Atom<'a>> {
    let mut entries = a.iter().filter(|a| a.tag == tag);
    let result = *entries.next().ok_or(AudioError::Unsupported)?;
    if entries.next().is_some() {
        return bad();
    }
    Ok(result)
}
fn allowed(a: &[Atom<'_>], tags: &[&[u8]]) -> Result<()> {
    if a.iter().any(|a| !tags.contains(&a.tag)) {
        bad()
    } else {
        Ok(())
    }
}
fn children(a: Atom<'_>) -> Result<Vec<Atom<'_>>> {
    atoms(a.p, a.start)
}
fn full(p: &[u8]) -> Result<()> {
    if p.get(..4) != Some(&[0, 0, 0, 0]) {
        bad()
    } else {
        Ok(())
    }
}
fn table<'a>(a: &[Atom<'a>], tag: &[u8], stride: usize) -> Result<&'a [u8]> {
    let p = one(a, tag)?.p;
    full(p)?;
    let n = u32be(p, 4)? as usize;
    if n == 0 || n > 16384 {
        return Err(AudioError::Limit);
    }
    if p.len() != 8 + n * stride {
        return bad();
    }
    Ok(&p[8..])
}
fn m4a(b: &[u8]) -> Result<Audio> {
    let top = atoms(b, 0)?;
    allowed(&top, &[b"ftyp", b"moov", b"mdat", b"free"])?;
    let ftyp = one(&top, b"ftyp")?.p;
    if ftyp.len() < 8 || ftyp.len() > 64 || ftyp.len() % 4 != 0 || ftyp.get(..4) != Some(b"M4A ") {
        return bad();
    }
    let mdat = one(&top, b"mdat")?;
    if mdat.p.is_empty() {
        return bad();
    }
    let moov = children(one(&top, b"moov")?)?;
    allowed(&moov, &[b"mvhd", b"trak", b"udta"])?;
    let mvhd = one(&moov, b"mvhd")?.p;
    full(mvhd)?;
    if mvhd.len() != 100 {
        return bad();
    }
    // Only the qualified Apple iTunSMPB priming metadata is accepted below.
    // No reference movie, fragmented movie, extra tracks, or edit lists.
    if moov.iter().filter(|a| a.tag == b"udta").count() > 1 {
        return bad();
    }
    for a in moov.iter().filter(|a| a.tag == b"udta") {
        if a.p.len() > 65536 {
            return Err(AudioError::Limit);
        }
    }
    let trak = children(one(&moov, b"trak")?)?;
    allowed(&trak, &[b"tkhd", b"mdia"])?;
    let tkhd = one(&trak, b"tkhd")?.p;
    if tkhd.len() != 84 || tkhd[0] != 0 || u32be(tkhd, 76)? != 0 || u32be(tkhd, 80)? != 0 {
        return bad();
    }
    let mdia = children(one(&trak, b"mdia")?)?;
    allowed(&mdia, &[b"mdhd", b"hdlr", b"minf"])?;
    let mdhd = one(&mdia, b"mdhd")?.p;
    full(mdhd)?;
    if mdhd.len() != 24 {
        return bad();
    }
    let rate = u32be(mdhd, 12)?;
    let frames = u32be(mdhd, 16)?;
    let mut seconds = duration(u64::from(frames), rate)?;
    let movie_rate = u32be(mvhd, 12)?;
    let movie_frames = u32be(mvhd, 16)?;
    if movie_rate == 0
        || movie_rate > 1_000_000
        || movie_frames == 0
        || (f64::from(movie_frames) / f64::from(movie_rate) - seconds).abs() > 0.002
        || u32be(tkhd, 20)? != movie_frames
    {
        return bad();
    }
    let hdlr = one(&mdia, b"hdlr")?.p;
    full(hdlr)?;
    if hdlr.len() > 256 || hdlr.get(8..12) != Some(b"soun") {
        return bad();
    }
    let minf = children(one(&mdia, b"minf")?)?;
    allowed(&minf, &[b"smhd", b"dinf", b"stbl"])?;
    if one(&minf, b"smhd")?.p != [0; 8] {
        return bad();
    }
    let dinf = children(one(&minf, b"dinf")?)?;
    allowed(&dinf, &[b"dref"])?;
    let dref = one(&dinf, b"dref")?;
    full(dref.p)?;
    if u32be(dref.p, 4)? != 1 {
        return bad();
    }
    let refs = atoms(
        dref.p.get(8..).ok_or(AudioError::Unsupported)?,
        dref.start + 8,
    )?;
    if refs.len() != 1 || refs[0].tag != b"url " || refs[0].p != [0, 0, 0, 1] {
        return bad();
    }
    let stbl = children(one(&minf, b"stbl")?)?;
    allowed(&stbl, &[b"stsd", b"stts", b"stsc", b"stsz", b"stco"])?;
    let stsd = one(&stbl, b"stsd")?;
    full(stsd.p)?;
    if u32be(stsd.p, 4)? != 1 {
        return bad();
    }
    let descriptions = atoms(
        stsd.p.get(8..).ok_or(AudioError::Unsupported)?,
        stsd.start + 8,
    )?;
    if descriptions.len() != 1 {
        return bad();
    }
    let desc = descriptions[0];
    let format = match desc.tag {
        b"mp4a" => Format::Aac,
        b"alac" => Format::Alac,
        _ => return bad(),
    };
    if desc.p.len() < 28
        || desc.p[..6] != [0; 6]
        || u16be(desc.p, 6)? != 1
        || desc.p[8..16] != [0; 8]
        || !(1..=2).contains(&u16be(desc.p, 16)?)
        || !matches!(u16be(desc.p, 18)?, 16 | 24 | 32)
    {
        return bad();
    }
    let extensions = atoms(&desc.p[28..], desc.start + 28)?;
    allowed(
        &extensions,
        if format == Format::Aac {
            &[b"esds", b"sbtd"]
        } else {
            &[b"alac"]
        },
    )?;
    let codec = one(
        &extensions,
        if format == Format::Aac {
            b"esds"
        } else {
            b"alac"
        },
    )?
    .p;
    if codec.len() < 12 || codec.len() > 256 {
        return bad();
    }
    full(codec)?;
    if format == Format::Aac {
        aac_config(codec, rate)?;
    } else {
        alac_config(codec, rate)?;
    }
    let actual_channels = if format == Format::Aac {
        let (es, _) = descriptor(&codec[4..], 3)?;
        let (c, _) = descriptor(&es[3..], 4)?;
        let (asc, _) = descriptor(&c[13..], 5)?;
        (u16be(asc, 0)? >> 3) & 15
    } else {
        u16::from(codec[13])
    };
    let entry_channels = u16be(desc.p, 16)?;
    let entry_bits = u16be(desc.p, 18)?;
    let entry_rate = u32be(desc.p, 24)?;
    // Qualified Apple encoders retain legacy stereo/44100 entry placeholders;
    // the finite validated codec cookie and mdhd remain authoritative.
    if (entry_channels != actual_channels && !(entry_channels == 2 && actual_channels == 1))
        || (Some(entry_rate) != rate.checked_mul(65536)
            && !(format == Format::Alac && entry_rate == 44100 << 16))
        || entry_bits
            != if format == Format::Aac {
                16
            } else {
                u16::from(codec[9])
            }
    {
        return bad();
    }
    for atom in extensions.iter().filter(|a| a.tag == b"sbtd") {
        if atom.p != [0, 0, 0, 0, b'I', b'1', b'6'] {
            return bad();
        }
    }
    if extensions.iter().any(|e| e.p.len() > 256) {
        return Err(AudioError::Limit);
    }
    if let Some(udta) = moov.iter().find(|a| a.tag == b"udta") {
        seconds = metadata_duration(
            *udta,
            frames,
            rate,
            format,
            if format == Format::Alac {
                u32be(codec, 4)?
            } else {
                1024
            },
        )?;
    }
    let timing = table(&stbl, b"stts", 8)?;
    let mut count = 0u64;
    let mut ticks = 0u64;
    for row in timing.chunks_exact(8) {
        let n = u64::from(u32be(row, 0)?);
        let d = u64::from(u32be(row, 4)?);
        if n == 0 || d == 0 {
            return bad();
        }
        count = count.checked_add(n).ok_or(AudioError::Limit)?;
        ticks = ticks
            .checked_add(n.checked_mul(d).ok_or(AudioError::Limit)?)
            .ok_or(AudioError::Limit)?;
    }
    if count > 16384 || ticks != u64::from(frames) {
        return bad();
    }
    let sizes = one(&stbl, b"stsz")?.p;
    full(sizes)?;
    let fixed = u32be(sizes, 4)?;
    let nsamples = u32be(sizes, 8)?;
    if u64::from(nsamples) != count
        || sizes.len() != 12 + if fixed == 0 { nsamples as usize * 4 } else { 0 }
    {
        return bad();
    }
    let chunks = table(&stbl, b"stco", 4)?;
    let mapping = table(&stbl, b"stsc", 12)?;
    let mut prior = 0;
    for row in mapping.chunks_exact(12) {
        let first = u32be(row, 0)?;
        if first <= prior
            || first as usize > chunks.len() / 4
            || u32be(row, 4)? == 0
            || u32be(row, 8)? != 1
        {
            return bad();
        }
        prior = first;
    }
    if u32be(mapping, 0)? != 1 {
        return bad();
    }
    let mut sample = 0usize;
    let mut map = 0usize;
    let mut last_end = mdat.start;
    for (i, row) in chunks.chunks_exact(4).enumerate() {
        while map + 12 < mapping.len() && u32be(mapping, map + 12)? as usize <= i + 1 {
            map += 12;
        }
        let n = u32be(mapping, map + 4)? as usize;
        let offset = u32be(row, 0)? as usize;
        if n > nsamples as usize || sample + n > nsamples as usize || offset < last_end {
            return bad();
        }
        let mut len = 0usize;
        for _ in 0..n {
            let size = if fixed == 0 {
                u32be(sizes, 12 + sample * 4)?
            } else {
                fixed
            } as usize;
            if size == 0 {
                return bad();
            }
            len = len.checked_add(size).ok_or(AudioError::Limit)?;
            sample += 1;
        }
        last_end = offset.checked_add(len).ok_or(AudioError::Limit)?;
        if offset < mdat.start || last_end > mdat.start + mdat.p.len() {
            return bad();
        }
    }
    if sample != nsamples as usize {
        return bad();
    }
    Ok(Audio {
        format,
        seconds,
        channels: actual_channels,
    })
}

fn descriptor(b: &[u8], tag: u8) -> Result<(&[u8], &[u8])> {
    if b.first() != Some(&tag) {
        return bad();
    }
    let mut at = 1;
    let mut len = 0usize;
    loop {
        let n = *b.get(at).ok_or(AudioError::Unsupported)?;
        at += 1;
        len = (len << 7) | usize::from(n & 127);
        if n & 128 == 0 {
            break;
        }
        if at == 5 {
            return bad();
        }
    }
    let end = at.checked_add(len).ok_or(AudioError::Unsupported)?;
    Ok((
        b.get(at..end).ok_or(AudioError::Unsupported)?,
        b.get(end..).ok_or(AudioError::Unsupported)?,
    ))
}
fn aac_config(p: &[u8], rate: u32) -> Result<()> {
    let (es, rest) = descriptor(&p[4..], 3)?;
    if !rest.is_empty() || es.len() < 3 || es[2] != 0 {
        return bad();
    } // no URL/dependency/OCR fields
    let (config, rest) = descriptor(&es[3..], 4)?;
    if config.len() < 13 || config[0] != 0x40 || config[1] & 0xfe != 0x14 {
        return bad();
    } // MPEG-4 audio
    let (asc, end) = descriptor(&config[13..], 5)?;
    if !end.is_empty() || asc.len() != 2 {
        return bad();
    }
    let bits = u16be(asc, 0)?;
    let index = ((bits >> 7) & 15) as usize;
    let rates = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    if bits >> 11 != 2
        || rates.get(index) != Some(&rate)
        || !matches!((bits >> 3) & 15, 1 | 2)
        || bits & 7 != 0
    {
        return bad();
    }
    let (sl, end) = descriptor(rest, 6)?;
    if sl != [2] || !end.is_empty() {
        return bad();
    }
    Ok(())
}
fn alac_config(p: &[u8], rate: u32) -> Result<()> {
    if p.len() != 28
        || !(1..=4096).contains(&u32be(p, 4)?)
        || p[8] != 0
        || !matches!(p[9], 16 | 24 | 32)
        || p[10] != 40
        || p[11] != 10
        || p[12] != 14
        || !(1..=2).contains(&p[13])
        || u16be(p, 14)? != 255
        || u32be(p, 16)? > 65536
        || u32be(p, 24)? != rate
    {
        return bad();
    }
    Ok(())
}

fn metadata_duration(
    udta: Atom<'_>,
    frames: u32,
    rate: u32,
    format: Format,
    packet: u32,
) -> Result<f64> {
    let a = children(udta)?;
    if a.len() != 1 {
        return bad();
    }
    let meta = one(&a, b"meta")?;
    full(meta.p)?;
    let a = atoms(&meta.p[4..], meta.start + 4)?;
    allowed(&a, &[b"hdlr", b"ilst"])?;
    let handler = one(&a, b"hdlr")?.p;
    full(handler)?;
    if handler.len() > 64 || handler.get(8..16) != Some(b"mdirappl") {
        return bad();
    }
    let items = children(one(&a, b"ilst")?)?;
    if items.len() != 1 || items[0].tag != b"----" {
        return bad();
    }
    let entry = children(items[0])?;
    allowed(&entry, &[b"mean", b"name", b"data"])?;
    let mean = one(&entry, b"mean")?.p;
    let name = one(&entry, b"name")?.p;
    full(mean)?;
    full(name)?;
    if &mean[4..] != b"com.apple.iTunes" || &name[4..] != b"iTunSMPB" {
        return bad();
    }
    let data = one(&entry, b"data")?.p;
    if data.get(..8) != Some(&[0, 0, 0, 1, 0, 0, 0, 0]) || data.len() > 256 {
        return bad();
    }
    let text = std::str::from_utf8(&data[8..]).map_err(|_| AudioError::Unsupported)?;
    if !text.bytes().all(|b| b == b' ' || b.is_ascii_hexdigit()) {
        return bad();
    }
    let fields = text.split_ascii_whitespace().collect::<Vec<_>>();
    if fields.len() != 12 || fields.iter().any(|s| s.len() > 16) {
        return bad();
    }
    let values = fields
        .iter()
        .map(|s| u64::from_str_radix(s, 16).map_err(|_| AudioError::Unsupported))
        .collect::<Result<Vec<_>>>()?;
    if values[0] != 0
        || values[1] > 4096
        || values[2] > 4096
        || values[4..].iter().any(|v| *v != 0)
        || values[3] > u64::from(frames)
        || if format == Format::Alac {
            values[1] != 0
                || values[3] != u64::from(frames)
                || values[2] != u64::from((packet - frames % packet) % packet)
        } else {
            values[1] + values[2] + values[3] != u64::from(frames)
        }
    {
        return bad();
    }
    duration(values[3], rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    const AAC: &[u8] = include_bytes!("../tests/fixtures/normalization-aac.m4a");
    const ALAC: &[u8] = include_bytes!("../tests/fixtures/normalization-alac.m4a");
    fn pcm(seconds: u32) -> Vec<u8> {
        let n = seconds * 44100 * 4;
        let mut b = b"RIFF".to_vec();
        b.extend((n + 36).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(44100u32.to_le_bytes());
        b.extend(176400u32.to_le_bytes());
        b.extend(4u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(n.to_le_bytes());
        b.resize(n as usize + 44, 0);
        b
    }
    fn aiff_fixture() -> Vec<u8> {
        let n = 44100 * 4;
        let mut b = b"FORM".to_vec();
        b.extend((n + 46u32).to_be_bytes());
        b.extend(b"AIFFCOMM");
        b.extend(18u32.to_be_bytes());
        b.extend(2u16.to_be_bytes());
        b.extend(44100u32.to_be_bytes());
        b.extend(16u16.to_be_bytes());
        b.extend([0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]);
        b.extend(b"SSND");
        b.extend((n + 8).to_be_bytes());
        b.extend([0; 8]);
        b.resize(n as usize + 54, 0);
        b
    }
    #[test]
    fn actual_formats_and_duration_limits() {
        let wave = pcm(1);
        let aiff = aiff_fixture();
        for (name, bytes, format) in [
            ("a.wav", wave.as_slice(), Format::Wav),
            ("a.aiff", aiff.as_slice(), Format::Aiff),
            ("a.m4a", AAC, Format::Aac),
            ("a.m4a", ALAC, Format::Alac),
        ] {
            let a = inspect(name, bytes).unwrap();
            assert_eq!(a.format, format);
            assert!(a.seconds > 0.0 && a.seconds < 10.0);
            assert!(!a.format.name().is_empty());
            assert!(inspect("wrong.mp3", bytes).is_err());
            for n in [0, 8, bytes.len() / 2, bytes.len() - 1] {
                assert!(inspect(name, &bytes[..n]).is_err());
            }
        }
        assert_eq!(
            inspect("huge.wav", &pcm(61)).unwrap_err(),
            AudioError::Limit
        );
        let mut long = pcm(1);
        long[24..28].copy_from_slice(&8000u32.to_le_bytes());
        long[28..32].copy_from_slice(&32000u32.to_le_bytes());
        long.resize(44 + 61 * 32000, 0);
        let n = (long.len() - 8) as u32;
        long[4..8].copy_from_slice(&n.to_le_bytes());
        let n = (long.len() - 44) as u32;
        long[40..44].copy_from_slice(&n.to_le_bytes());
        assert_eq!(inspect("long.wav", &long).unwrap_err(), AudioError::Limit);
    }
    fn pos(b: &[u8], needle: &[u8]) -> usize {
        b.windows(needle.len()).position(|p| p == needle).unwrap()
    }
    #[test]
    fn references_codecs_tracks_and_sample_extents_fail_closed() {
        let dref = pos(AAC, b"url ");
        let esds = pos(AAC, b"esds");
        let sample = pos(AAC, b"mp4a");
        let mut variants = Vec::new();
        for (at, value) in [
            (dref + 7, 0),
            (esds + 15, 0x40),
            (esds + 21, 0x20),
            (sample + 10, 1),
            (sample + 11, 2),
            (sample + 21, 3),
            (sample + 28, 1),
        ] {
            let mut b = AAC.to_vec();
            b[at] = value;
            variants.push(b);
        }
        let mut video = AAC.to_vec();
        let h = pos(&video, b"soun");
        video[h..h + 4].copy_from_slice(b"vide");
        variants.push(video);
        let mut external = AAC.to_vec();
        let h = pos(&external, b"dref");
        external[h + 8..h + 12].copy_from_slice(&2u32.to_be_bytes());
        variants.push(external);
        let mut outside = AAC.to_vec();
        let h = pos(&outside, b"stco");
        outside[h + 12..h + 16].copy_from_slice(&u32::MAX.to_be_bytes());
        variants.push(outside);
        let mut missing = AAC.to_vec();
        let h = pos(&missing, b"stsz");
        missing[h + 12..h + 16].copy_from_slice(&u32::MAX.to_be_bytes());
        variants.push(missing);
        for (index, b) in variants.into_iter().enumerate() {
            assert!(inspect("a.m4a", &b).is_err(), "variant {index}");
        }
        for at in [4, 8, 9, 10, 11, 12, 13, 14, 24] {
            let mut b = ALAC.to_vec();
            let a = pos(&b[pos(&b, b"alac") + 4..], b"alac") + pos(&b, b"alac") + 8;
            b[a + at] = 255;
            assert!(inspect("a.m4a", &b).is_err());
        }
    }
    #[test]
    fn descriptors_reject_url_dependency_ocr_unsupported_aac_and_oversized_lengths() {
        let e = pos(AAC, b"esds");
        let size = u32be(AAC, e - 4).unwrap() as usize;
        let cookie = &AAC[e + 4..e - 4 + size];
        aac_config(cookie, 16000).unwrap();
        for flags in [0x80, 0x40, 0x20, 1] {
            let mut b = cookie.to_vec();
            b[11] = flags;
            assert!(aac_config(&b, 16000).is_err());
        }
        let mut bad = cookie.to_vec();
        bad[17] = 0x20;
        assert!(aac_config(&bad, 16000).is_err());
        for bytes in [
            &[3, 0xff, 0xff, 0xff, 0xff][..],
            &[3, 20, 0][..],
            &[4, 0][..],
        ] {
            assert!(descriptor(bytes, 3).is_err());
        }
        assert!(inspect("x.m4a", &vec![0; MAX + 1]).is_err());
    }
}
