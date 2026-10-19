//! MPEG-DASH MPD parsing and segment expansion.

use roxmltree::{Document, Node};
use url::Url;

use crate::model::{drm_error, live_error, resolve, select_index, ByteRange, Item, Plan, Track};
use crate::{Error, Quality, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Video,
    Audio,
}

#[derive(Debug, Clone)]
pub(crate) struct DashRep {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub ext: &'static str,
    /// Init section first (if any), then media segments.
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DashPeriod {
    pub video: Vec<DashRep>,
    pub audio: Vec<DashRep>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Mpd {
    pub periods: Vec<DashPeriod>,
}

/// Per-representation segment cap (e.g. ~111 h of 2 s segments).
const MAX_SEGMENTS: usize = 200_000;

fn err(msg: impl Into<String>) -> Error {
    Error::Other(msg.into())
}

fn children<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = Node<'a, 'i>> {
    n.children()
        .filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn child<'a, 'i>(n: Node<'a, 'i>, name: &'static str) -> Option<Node<'a, 'i>> {
    children(n, name).next()
}

fn with_base(n: Node, parent: &Url) -> Result<Url> {
    match child(n, "BaseURL")
        .and_then(|b| b.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(t) => parent
            .join(t)
            .map_err(|e| err(format!("bad BaseURL {t:?}: {e}"))),
        None => Ok(parent.clone()),
    }
}

/// ISO 8601 duration (`PT1H2M3.5S`, `P1DT2H`) in seconds.
pub(crate) fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let rest = s.strip_prefix('P')?;
    let (date, time) = match rest.split_once('T') {
        Some((d, t)) => (d, t),
        None => (rest, ""),
    };
    let mut total = 0.0;
    let mut num = String::new();
    for c in date.chars() {
        match c {
            'Y' => total += num.parse::<f64>().ok()? * 365.0 * 86400.0,
            'M' => total += num.parse::<f64>().ok()? * 30.0 * 86400.0,
            'W' => total += num.parse::<f64>().ok()? * 7.0 * 86400.0,
            'D' => total += num.parse::<f64>().ok()? * 86400.0,
            _ => {
                num.push(c);
                continue;
            }
        }
        num.clear();
    }
    if !num.is_empty() {
        return None;
    }
    for c in time.chars() {
        match c {
            'H' => total += num.parse::<f64>().ok()? * 3600.0,
            'M' => total += num.parse::<f64>().ok()? * 60.0,
            'S' => total += num.parse::<f64>().ok()?,
            _ => {
                num.push(c);
                continue;
            }
        }
        num.clear();
    }
    if !num.is_empty() || !total.is_finite() || total < 0.0 {
        return None;
    }
    Some(total)
}

/// `a-b` (inclusive) → ByteRange.
fn parse_range(s: &str) -> Result<ByteRange> {
    let (a, b) = s
        .trim()
        .split_once('-')
        .ok_or_else(|| err(format!("bad range {s:?}")))?;
    let a: u64 = a
        .trim()
        .parse()
        .map_err(|_| err(format!("bad range {s:?}")))?;
    let b: u64 = b
        .trim()
        .parse()
        .map_err(|_| err(format!("bad range {s:?}")))?;
    let length = b
        .checked_sub(a)
        .and_then(|d| d.checked_add(1))
        .filter(|len| a.checked_add(*len).is_some())
        .ok_or_else(|| err(format!("bad range {s:?}")))?;
    Ok(ByteRange { offset: a, length })
}

/// Expand `$RepresentationID$`, `$Number$`, `$Number%05d$`, `$Bandwidth$`, `$Time$`, `$$`.
pub(crate) fn substitute(
    tpl: &str,
    rep_id: &str,
    number: u64,
    bandwidth: u64,
    time: u64,
) -> Result<String> {
    let mut out = String::with_capacity(tpl.len() + 16);
    let mut rest = tpl;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('$')
            .ok_or_else(|| err(format!("unterminated $ in template {tpl:?}")))?;
        let token = &after[..end];
        rest = &after[end + 1..];
        if token.is_empty() {
            out.push('$');
            continue;
        }
        let (ident, fmt) = match token.find('%') {
            Some(i) => (&token[..i], Some(&token[i..])),
            None => (token, None),
        };
        let value = match ident {
            "RepresentationID" => {
                out.push_str(rep_id);
                continue;
            }
            "Number" => number,
            "Bandwidth" => bandwidth,
            "Time" => time,
            _ => return Err(err(format!("unknown template identifier ${token}$"))),
        };
        match fmt {
            None => out.push_str(&value.to_string()),
            Some(f) => {
                // %0<width>d (also %0<width>x/X / %d)
                let spec = f.trim_start_matches('%');
                let (digits, conv) = match spec.char_indices().last() {
                    Some((i, c)) => (&spec[..i], c),
                    None => ("", 'd'),
                };
                if !matches!(conv, 'd' | 'i' | 'u' | 'x' | 'X')
                    || !digits.chars().all(|c| c.is_ascii_digit())
                {
                    return Err(err(format!("bad format in template identifier ${token}$")));
                }
                let width: usize = digits
                    .trim_start_matches('0')
                    .parse::<usize>()
                    .unwrap_or(if digits.trim_start_matches('0').is_empty() {
                        0
                    } else {
                        32
                    })
                    .min(32);
                let s = match conv {
                    'x' => format!("{value:x}"),
                    'X' => format!("{value:X}"),
                    _ => value.to_string(),
                };
                out.push_str(&format!("{s:0>width$}"));
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineEntry {
    pub t: Option<u64>,
    pub d: u64,
    pub r: i64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Template {
    pub media: Option<String>,
    pub initialization: Option<String>,
    pub start_number: u64,
    pub end_number: Option<u64>,
    pub timescale: u64,
    pub duration: Option<u64>,
    pub presentation_time_offset: u64,
    pub timeline: Option<Vec<TimelineEntry>>,
}

impl Template {
    /// `levels` are SegmentTemplate nodes ordered most specific first.
    fn merge(levels: &[Node]) -> Result<Self> {
        let get = |name: &str| levels.iter().find_map(|n| n.attribute(name));
        let num = |name: &str| -> Result<Option<u64>> {
            get(name)
                .map(|v| {
                    v.trim()
                        .parse::<u64>()
                        .map_err(|_| err(format!("bad {name}={v:?}")))
                })
                .transpose()
        };
        let initialization = get("initialization").map(str::to_string).or_else(|| {
            levels
                .iter()
                .find_map(|n| child(*n, "Initialization").and_then(|i| i.attribute("sourceURL")))
                .map(str::to_string)
        });
        let timeline = match levels.iter().find_map(|n| child(*n, "SegmentTimeline")) {
            Some(tl) => {
                let mut v = Vec::new();
                for s in children(tl, "S") {
                    let p = |name: &str| -> Result<Option<i64>> {
                        s.attribute(name)
                            .map(|x| {
                                x.trim()
                                    .parse::<i64>()
                                    .map_err(|_| err(format!("bad S@{name}")))
                            })
                            .transpose()
                    };
                    let d = p("d")?.ok_or_else(|| err("S without d"))?;
                    if d <= 0 {
                        return Err(err("S@d must be positive"));
                    }
                    v.push(TimelineEntry {
                        t: p("t")?.map(|t| t.max(0) as u64),
                        d: d as u64,
                        r: p("r")?.unwrap_or(0),
                    });
                }
                Some(v)
            }
            None => None,
        };
        Ok(Template {
            media: get("media").map(str::to_string),
            initialization,
            start_number: num("startNumber")?.unwrap_or(1),
            end_number: num("endNumber")?,
            timescale: num("timescale")?.unwrap_or(1).max(1),
            duration: num("duration")?,
            presentation_time_offset: num("presentationTimeOffset")?.unwrap_or(0),
            timeline,
        })
    }

    /// `(number, time)` for every segment of the period.
    pub fn segment_times(&self, period_duration: Option<f64>) -> Result<Vec<(u64, u64)>> {
        let mut out = Vec::new();
        let period_end = period_duration.map(|d| {
            self.presentation_time_offset
                .saturating_add((d * self.timescale as f64).round() as u64)
        });
        if let Some(tl) = &self.timeline {
            let mut t = 0u64;
            let mut number = self.start_number;
            for (i, s) in tl.iter().enumerate() {
                if let Some(st) = s.t {
                    t = st;
                }
                let repeats = if s.r >= 0 {
                    s.r as u64
                } else {
                    let end = tl
                        .get(i + 1)
                        .and_then(|n| n.t)
                        .or(period_end)
                        .ok_or_else(|| err("open-ended SegmentTimeline without period duration"))?;
                    end.saturating_sub(t).div_ceil(s.d).saturating_sub(1)
                };
                for _ in 0..=repeats {
                    out.push((number, t));
                    t = t.saturating_add(s.d);
                    number = number.wrapping_add(1);
                    if out.len() > MAX_SEGMENTS {
                        return Err(err("SegmentTimeline too large"));
                    }
                }
            }
        } else {
            let duration = self
                .duration
                .filter(|d| *d > 0)
                .ok_or_else(|| err("SegmentTemplate without duration or SegmentTimeline"))?;
            let count = match self.end_number {
                Some(end) => end.saturating_sub(self.start_number).saturating_add(1),
                None => {
                    let pd = period_duration.ok_or_else(|| err("unknown period duration"))?;
                    let n = pd * self.timescale as f64 / duration as f64;
                    if !n.is_finite() {
                        return Err(err("too many segments"));
                    }
                    (n - 1e-6).ceil().max(0.0) as u64
                }
            };
            if count > MAX_SEGMENTS as u64 {
                return Err(err("too many segments"));
            }
            for i in 0..count {
                out.push((
                    self.start_number.wrapping_add(i),
                    self.presentation_time_offset
                        .saturating_add(i.saturating_mul(duration)),
                ));
            }
        }
        Ok(out)
    }

    fn items(
        &self,
        base: &Url,
        rep_id: &str,
        bandwidth: u64,
        period_duration: Option<f64>,
    ) -> Result<Vec<Item>> {
        let media = self
            .media
            .as_deref()
            .ok_or_else(|| err("SegmentTemplate without media"))?;
        let mut items = Vec::new();
        if let Some(init) = &self.initialization {
            let u = substitute(init, rep_id, self.start_number, bandwidth, 0)?;
            items.push(Item {
                init: true,
                ..Item::plain(resolve(base, &u)?)
            });
        }
        for (number, time) in self.segment_times(period_duration)? {
            let u = substitute(media, rep_id, number, bandwidth, time)?;
            items.push(Item::plain(resolve(base, &u)?));
        }
        Ok(items)
    }
}

fn segment_list_items(list: Node, base: &Url) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    if let Some(init) = child(list, "Initialization") {
        let url = match init.attribute("sourceURL") {
            Some(u) => resolve(base, u)?,
            None => base.to_string(),
        };
        let range = init.attribute("range").map(parse_range).transpose()?;
        items.push(Item {
            url,
            range,
            key: None,
            init: true,
        });
    }
    for s in children(list, "SegmentURL") {
        let url = match s.attribute("media") {
            Some(u) => resolve(base, u)?,
            None => base.to_string(),
        };
        let range = s.attribute("mediaRange").map(parse_range).transpose()?;
        items.push(Item {
            url,
            range,
            key: None,
            init: false,
        });
    }
    Ok(items)
}

fn kind_of(adaptation: Node, rep: Node) -> Option<ContentKind> {
    let from = |s: &str| {
        let s = s.to_ascii_lowercase();
        if s.starts_with("video") {
            Some(ContentKind::Video)
        } else if s.starts_with("audio") {
            Some(ContentKind::Audio)
        } else {
            None
        }
    };
    for (node, attr) in [
        (rep, "contentType"),
        (rep, "mimeType"),
        (adaptation, "contentType"),
        (adaptation, "mimeType"),
    ] {
        if let Some(v) = node.attribute(attr) {
            return from(v);
        }
    }
    let codecs = rep
        .attribute("codecs")
        .or_else(|| adaptation.attribute("codecs"))?
        .to_ascii_lowercase();
    if ["avc", "hvc", "hev", "vp0", "vp8", "vp9", "av01", "dvh"]
        .iter()
        .any(|p| codecs.starts_with(p))
    {
        Some(ContentKind::Video)
    } else if ["mp4a", "opus", "ac-3", "ec-3", "flac", "vorbis"]
        .iter()
        .any(|p| codecs.starts_with(p))
    {
        Some(ContentKind::Audio)
    } else {
        None
    }
}

pub(crate) fn parse_mpd(text: &str, manifest_url: &Url) -> Result<Mpd> {
    let doc = Document::parse(text).map_err(|e| err(format!("invalid MPD: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "MPD" {
        return Err(err("not a DASH manifest (root element is not MPD)"));
    }
    if doc
        .descendants()
        .any(|n| n.is_element() && n.tag_name().name() == "ContentProtection")
    {
        return Err(drm_error());
    }
    if root
        .attribute("type")
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("dynamic"))
    {
        return Err(live_error());
    }
    let mpd_duration = root
        .attribute("mediaPresentationDuration")
        .and_then(parse_duration);
    let mpd_base = with_base(root, manifest_url)?;

    let period_nodes: Vec<Node> = children(root, "Period").collect();
    let starts: Vec<Option<f64>> = period_nodes
        .iter()
        .map(|p| p.attribute("start").and_then(parse_duration))
        .collect();
    let mut mpd = Mpd::default();
    let mut total_items = 0usize;
    let mut prev_end = Some(0.0);
    for (pi, period) in period_nodes.iter().enumerate() {
        let start = starts[pi].or(prev_end).unwrap_or(0.0);
        let duration = period
            .attribute("duration")
            .and_then(parse_duration)
            .or_else(|| {
                starts
                    .get(pi + 1)
                    .copied()
                    .flatten()
                    .map(|next| next - start)
            })
            .or_else(|| mpd_duration.map(|d| d - start));
        prev_end = duration.map(|d| start + d);
        let period_base = with_base(*period, &mpd_base)?;
        let mut out = DashPeriod::default();
        for aset in children(*period, "AdaptationSet") {
            let as_base = with_base(aset, &period_base)?;
            for rep in children(aset, "Representation") {
                let Some(kind) = kind_of(aset, rep) else {
                    continue;
                };
                let rep_base = with_base(rep, &as_base)?;
                let id = rep.attribute("id").unwrap_or("").to_string();
                let bandwidth = rep
                    .attribute("bandwidth")
                    .and_then(|b| b.trim().parse().ok())
                    .unwrap_or(0);
                let dim = |a: &str| {
                    rep.attribute(a)
                        .or_else(|| aset.attribute(a))
                        .and_then(|v| v.trim().parse().ok())
                };

                let levels = [rep, aset, *period];
                let templates: Vec<Node> = levels
                    .iter()
                    .filter_map(|n| child(*n, "SegmentTemplate"))
                    .collect();
                let items = if !templates.is_empty() {
                    Template::merge(&templates)?.items(&rep_base, &id, bandwidth, duration)?
                } else if let Some(list) = levels.iter().find_map(|n| child(*n, "SegmentList")) {
                    segment_list_items(list, &rep_base)?
                } else {
                    // SegmentBase or a bare BaseURL: one self-contained file.
                    vec![Item::plain(rep_base.to_string())]
                };
                total_items += items.len();
                if total_items > 10 * MAX_SEGMENTS {
                    return Err(err("MPD describes too many segments"));
                }
                if items.iter().all(|i| i.init) {
                    continue;
                }
                let mime = rep
                    .attribute("mimeType")
                    .or_else(|| aset.attribute("mimeType"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let ext = if mime.contains("webm") {
                    ".webm"
                } else if mime.contains("mp2t") {
                    ".ts"
                } else {
                    ".mp4"
                };
                let r = DashRep {
                    id,
                    bandwidth,
                    width: dim("width"),
                    height: dim("height"),
                    codecs: rep
                        .attribute("codecs")
                        .or_else(|| aset.attribute("codecs"))
                        .map(str::to_string),

                    ext,
                    items,
                };
                match kind {
                    ContentKind::Video => out.video.push(r),
                    ContentKind::Audio => out.audio.push(r),
                }
            }
        }
        if !out.video.is_empty() || !out.audio.is_empty() {
            mpd.periods.push(out);
        }
    }
    if mpd.periods.is_empty() {
        return Err(err("MPD contains no playable representations"));
    }
    Ok(mpd)
}

impl Mpd {
    /// Video representations of the first period (audio ones for audio-only manifests).
    pub fn variant_reps(&self) -> &[DashRep] {
        let p = &self.periods[0];
        if p.video.is_empty() {
            &p.audio
        } else {
            &p.video
        }
    }

    pub fn plan(&self, quality: Quality) -> Result<Plan> {
        let mut video: Option<Track> = None;
        let mut audio: Option<Track> = None;
        let append = |slot: &mut Option<Track>, rep: &DashRep| match slot {
            Some(t) => t.items.extend(rep.items.iter().cloned()),
            None => {
                *slot = Some(Track {
                    items: rep.items.clone(),
                    ext: rep.ext,
                })
            }
        };
        for p in &self.periods {
            if !p.video.is_empty() {
                let cands: Vec<_> = p.video.iter().map(|r| (r.height, r.bandwidth)).collect();
                let Some(i) = select_index(&cands, quality) else {
                    continue;
                };
                append(&mut video, &p.video[i]);
            }
            if let Some(a) = p.audio.iter().max_by_key(|r| r.bandwidth) {
                append(&mut audio, a);
            }
        }
        match (video, audio) {
            (Some(v), a) => Ok(Plan { video: v, audio: a }),
            (None, Some(a)) => Ok(Plan {
                video: a,
                audio: None,
            }),
            (None, None) => Err(err("MPD contains no playable representations")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/content/manifest.mpd").unwrap()
    }

    const NUMBER_MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT9.5S" minBufferTime="PT2S">
  <BaseURL>media/</BaseURL>
  <Period id="p0">
    <AdaptationSet mimeType="video/mp4" segmentAlignment="true">
      <SegmentTemplate timescale="1000" duration="2000" startNumber="1"
        initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number%04d$-$Bandwidth$.m4s"/>
      <Representation id="v360" bandwidth="800000" width="640" height="360" codecs="avc1.4d401e"/>
      <Representation id="v1080" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028"/>
      <Representation id="v720" bandwidth="2500000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="en">
      <Representation id="a64" bandwidth="64000" codecs="mp4a.40.2">
        <SegmentTemplate timescale="48000" duration="96000" startNumber="0" initialization="a/init.mp4" media="a/$Number$.m4s"/>
      </Representation>
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2">
        <SegmentTemplate timescale="48000" duration="96000" startNumber="0" initialization="a/init-$RepresentationID$.mp4" media="a/$RepresentationID$-$Number$.m4s"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="image/jpeg"><Representation id="thumbs" bandwidth="1000"/></AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration("PT9.5S"), Some(9.5));
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723.0));
        assert_eq!(parse_duration("P1DT1S"), Some(86401.0));
        assert_eq!(parse_duration("PT0H1M59.89S"), Some(119.89));
        assert_eq!(parse_duration("garbage"), None);
    }

    #[test]
    fn template_substitution() {
        assert_eq!(
            substitute("$RepresentationID$/$Number%05d$.m4s", "v1", 42, 0, 0).unwrap(),
            "v1/00042.m4s"
        );
        assert_eq!(
            substitute("t$Time$-b$Bandwidth$-$$", "x", 1, 300, 9000).unwrap(),
            "t9000-b300-$"
        );
        assert!(substitute("$Nope$", "x", 1, 1, 1).is_err());
    }

    #[test]
    fn segment_template_number() {
        let mpd = parse_mpd(NUMBER_MPD, &base()).unwrap();
        let p = &mpd.periods[0];
        assert_eq!(p.video.len(), 3);
        assert_eq!(p.audio.len(), 2);
        let v = &p.video[0];
        // ceil(9.5 / 2) = 5 segments + init
        assert_eq!(v.items.len(), 6);
        assert!(v.items[0].init);
        assert_eq!(
            v.items[0].url,
            "https://cdn.example.com/content/media/v360/init.mp4"
        );
        assert_eq!(
            v.items[1].url,
            "https://cdn.example.com/content/media/v360/seg-0001-800000.m4s"
        );
        assert_eq!(
            v.items[5].url,
            "https://cdn.example.com/content/media/v360/seg-0005-800000.m4s"
        );
        assert_eq!(
            p.audio[1].items[1].url,
            "https://cdn.example.com/content/media/a/a128-0.m4s"
        );

        let plan = mpd.plan(Quality::Best).unwrap();
        assert!(plan.video.items[1].url.contains("v1080"));
        assert!(plan.audio.as_ref().unwrap().items[0]
            .url
            .ends_with("init-a128.mp4"));
        let plan = mpd.plan(Quality::MaxHeight(720)).unwrap();
        assert!(plan.video.items[1].url.contains("v720"));
        assert_eq!(plan.video.ext, ".mp4");
        let plan = mpd.plan(Quality::MaxHeight(400)).unwrap();
        assert!(plan.video.items[1].url.contains("v360"));
        assert_eq!(mpd.variant_reps().len(), 3);
    }

    #[test]
    fn segment_template_timeline() {
        let text = r#"<MPD type="static" mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet contentType="video">
      <Representation id="v" bandwidth="1" height="240">
        <BaseURL>https://other.example.com/x/</BaseURL>
        <SegmentTemplate timescale="10" media="$RepresentationID$_$Time$.m4s" initialization="init.m4s">
          <SegmentTimeline>
            <S t="0" d="20" r="1"/>
            <S d="15"/>
            <S t="60" d="10" r="-1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let mpd = parse_mpd(text, &base()).unwrap();
        let urls: Vec<_> = mpd.periods[0].video[0]
            .items
            .iter()
            .map(|i| i.url.rsplit('/').next().unwrap().to_string())
            .collect();
        // 0,20 (r=1), 40 (d=15), then 60,70,80,90 until period end at 100.
        assert_eq!(
            urls,
            [
                "init.m4s", "v_0.m4s", "v_20.m4s", "v_40.m4s", "v_60.m4s", "v_70.m4s", "v_80.m4s",
                "v_90.m4s"
            ]
        );
        assert!(mpd.periods[0].video[0].items[1]
            .url
            .starts_with("https://other.example.com/x/"));

        let tpl = Template {
            start_number: 5,
            timescale: 1,
            timeline: Some(vec![TimelineEntry {
                t: Some(100),
                d: 4,
                r: 2,
            }]),
            ..Default::default()
        };
        assert_eq!(
            tpl.segment_times(None).unwrap(),
            [(5, 100), (6, 104), (7, 108)]
        );
    }

    #[test]
    fn segment_list_and_base() {
        let text = r#"<MPD type="static" mediaPresentationDuration="PT4S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v" bandwidth="100" height="480">
        <BaseURL>video.mp4</BaseURL>
        <SegmentList>
          <Initialization range="0-99"/>
          <SegmentURL mediaRange="100-199"/>
          <SegmentURL media="seg2.mp4"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/webm">
      <Representation id="a" bandwidth="100">
        <BaseURL>audio.webm</BaseURL>
        <SegmentBase indexRange="0-500"><Initialization range="0-100"/></SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let mpd = parse_mpd(text, &base()).unwrap();
        let v = &mpd.periods[0].video[0];
        assert_eq!(v.items[0].url, "https://cdn.example.com/content/video.mp4");
        assert_eq!(
            v.items[0].range,
            Some(ByteRange {
                offset: 0,
                length: 100
            })
        );
        assert_eq!(
            v.items[1].range,
            Some(ByteRange {
                offset: 100,
                length: 100
            })
        );
        assert_eq!(v.items[2].url, "https://cdn.example.com/content/seg2.mp4");
        let a = &mpd.periods[0].audio[0];
        assert_eq!(a.items.len(), 1);
        assert_eq!(a.items[0].url, "https://cdn.example.com/content/audio.webm");
        assert_eq!(a.ext, ".webm");
    }

    #[test]
    fn drm_and_live_detection() {
        let drm = r#"<MPD type="static" mediaPresentationDuration="PT4S"><Period><AdaptationSet mimeType="video/mp4">
            <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
            <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
            <Representation id="v" bandwidth="1"><SegmentTemplate duration="1" media="$Number$.m4s"/></Representation>
            </AdaptationSet></Period></MPD>"#;
        assert!(
            matches!(parse_mpd(drm, &base()), Err(Error::Other(m)) if m == "DRM-protected stream")
        );
        let live = r#"<MPD type="dynamic"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v" bandwidth="1"/></AdaptationSet></Period></MPD>"#;
        assert!(matches!(parse_mpd(live, &base()), Err(Error::Other(m)) if m.contains("live")));
        assert!(parse_mpd("<html/>", &base()).is_err());
    }
}
