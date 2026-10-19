//! Fuzz-ish robustness tests: crafted and random manifests must yield errors, never panics.

use url::Url;

use crate::{dash, hls, Quality};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn base() -> Url {
    Url::parse("https://example.com/a/b/index").unwrap()
}

const HLS_TOKENS: &[&str] = &[
    "#EXTM3U\n",
    "#EXTINF:4,\n",
    "#EXT-X-ENDLIST\n",
    "#EXT-X-BYTERANGE:",
    "#EXT-X-MAP:URI=\"",
    "#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x",
    "#EXT-X-KEY:METHOD=",
    "#EXT-X-MEDIA-SEQUENCE:",
    "#EXT-X-STREAM-INF:BANDWIDTH=",
    ",RESOLUTION=",
    "x",
    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"g\",URI=\"",
    ",AUDIO=\"g\"",
    "\"",
    ",",
    "@",
    "=",
    "0",
    "1",
    "18446744073709551615",
    "18446744073709551616",
    "-1",
    "seg.ts\n",
    "http://[::1\n",
    "é",
    "日本",
    "\u{feff}",
    "\n",
    " ",
    "%",
    "$",
    "//x",
    "../../..",
    "\r\n",
    "#EXT-X-PLAYLIST-TYPE:VOD\n",
    "#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES\n",
    "ZZZZ",
];

const MPD_TOKENS: &[&str] = &[
    "<MPD type=\"static\" mediaPresentationDuration=\"PT",
    "S\">",
    "<Period",
    " duration=\"PT",
    "\">",
    "</Period>",
    "<AdaptationSet mimeType=\"video/mp4\">",
    "<AdaptationSet contentType=\"audio\">",
    "</AdaptationSet>",
    "<Representation id=\"",
    "\" bandwidth=\"",
    "\" height=\"",
    "\">",
    "</Representation>",
    "<SegmentTemplate media=\"",
    "$Number%0",
    "d$",
    "$Time$",
    "$RepresentationID$",
    "$",
    "$$",
    "%",
    "é",
    "\" initialization=\"i\" timescale=\"",
    "\" duration=\"",
    "\" startNumber=\"",
    "\"/>",
    "<SegmentTimeline>",
    "<S t=\"",
    "\" d=\"",
    "\" r=\"",
    "</SegmentTimeline>",
    "</SegmentTemplate>",
    "<SegmentList>",
    "<SegmentURL mediaRange=\"",
    "-",
    "</SegmentList>",
    "<BaseURL>",
    "</BaseURL>",
    "0",
    "1",
    "-1",
    "3",
    "18446744073709551615",
    "9223372036854775807",
    "inf",
    "NaN",
    "</MPD>",
];

fn random_doc(rng: &mut Rng, tokens: &[&str], n: usize) -> String {
    (0..n).map(|_| rng.pick(tokens)).collect()
}

#[test]
fn random_hls_never_panics() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..20_000 {
        let len = (rng.next() % 40) as usize;
        let doc = format!("#EXTM3U\n{}", random_doc(&mut rng, HLS_TOKENS, len));
        let _ = hls::is_master(&doc);
        let _ = hls::parse_master(&doc, &base());
        if let Ok(p) = hls::parse_media(&doc, &base()) {
            let _ = p.into_track();
        }
        let _ = hls::parse_attributes(&doc);
    }
}

#[test]
fn random_mpd_never_panics() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20_000 {
        let len = (rng.next() % 60) as usize;
        let doc = random_doc(&mut rng, MPD_TOKENS, len);
        if let Ok(m) = dash::parse_mpd(&doc, &base()) {
            let _ = m.plan(Quality::Best);
            let _ = m.plan(Quality::MaxHeight(0));
        }
    }
}

#[test]
fn random_templates_never_panic() {
    let mut rng = Rng(42);
    for _ in 0..50_000 {
        let len = (rng.next() % 12) as usize;
        let t = random_doc(&mut rng, MPD_TOKENS, len);
        let _ = dash::substitute(&t, "r", u64::MAX, u64::MAX, u64::MAX);
    }
}

#[test]
fn crafted_hls_inputs() {
    let b = base();
    // Zero-length / overflowing byte ranges.
    for br in [
        "0@5",
        "18446744073709551615@1",
        "1@18446744073709551615",
        "5@x",
        "@",
        "",
    ] {
        let doc = format!("#EXTM3U\n#EXTINF:1,\n#EXT-X-BYTERANGE:{br}\na.ts\n#EXT-X-ENDLIST\n");
        assert!(hls::parse_media(&doc, &b).is_err(), "{br}");
    }
    // Continuation byte range after an end at u64::MAX - 1.
    let doc = "#EXTM3U\n#EXT-X-BYTERANGE:18446744073709551614@0\na.ts\n#EXT-X-BYTERANGE:5\na.ts\n#EXT-X-ENDLIST\n";
    assert!(hls::parse_media(doc, &b).is_err());
    // Huge media sequence must not overflow when computing IVs.
    let doc = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:18446744073709551615\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:1,\na.ts\n#EXTINF:1,\nb.ts\n#EXT-X-ENDLIST\n";
    assert_eq!(hls::parse_media(doc, &b).unwrap().items.len(), 2);
    // Non-ASCII junk in attributes and IVs.
    let doc =
        "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xé1\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
    assert!(hls::parse_media(doc, &b).is_err());
    let doc = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=é,RESOLUTION=日x本,CODECS=\"\nv.m3u8\n";
    assert_eq!(hls::parse_master(doc, &b).unwrap().variants.len(), 1);
    assert!(hls::parse_media("", &b).is_err());
    assert!(hls::parse_media("\u{feff}", &b).is_err());
}

#[test]
fn crafted_dash_inputs() {
    let b = base();
    for t in [
        "$Number%0é$",
        "$Number%é$",
        "$Number%$",
        "$Number%099999999999999999999d$",
        "$Number%0-5d$",
        "$Number",
        "$Time%05s$",
    ] {
        let _ = dash::substitute(t, "r", 1, 1, 1);
    }
    // Width is capped.
    let s = dash::substitute("$Number%0999d$", "r", 7, 0, 0).unwrap();
    assert_eq!(s.len(), 32);
    assert!(dash::substitute("$Number%0é$", "r", 1, 1, 1).is_err());

    assert_eq!(dash::parse_duration("PTinfS"), None);
    assert_eq!(dash::parse_duration("PTNaNS"), None);
    assert_eq!(dash::parse_duration("PT-5S"), None);
    assert_eq!(dash::parse_duration("P1e308Y"), None);

    let mpd = |inner: &str| {
        format!(
            r#"<MPD type="static" mediaPresentationDuration="PT10S"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v" bandwidth="1">{inner}</Representation></AdaptationSet></Period></MPD>"#
        )
    };
    let cases = [
        // r=-1 over an enormous period.
        r#"<SegmentTemplate media="$Number$" timescale="1"><SegmentTimeline><S t="0" d="1" r="-1"/></SegmentTimeline></SegmentTemplate>"#.to_string(),
        r#"<SegmentTemplate media="$Time$" timescale="1"><SegmentTimeline><S t="18446744073709551615" d="9223372036854775807" r="5"/></SegmentTimeline></SegmentTemplate>"#.to_string(),
        r#"<SegmentTemplate media="$Number$" timescale="18446744073709551615" duration="1"/>"#.to_string(),
        r#"<SegmentTemplate media="$Number$" duration="1" startNumber="18446744073709551615" endNumber="18446744073709551615"/>"#.to_string(),
        r#"<SegmentTemplate media="$Number$" duration="1" startNumber="0" endNumber="18446744073709551615"/>"#.to_string(),
        r#"<SegmentTemplate media="$Number$" duration="0"/>"#.to_string(),
        r#"<SegmentList><SegmentURL mediaRange="0-18446744073709551615"/></SegmentList>"#.to_string(),
        r#"<SegmentList><SegmentURL mediaRange="10-5"/></SegmentList>"#.to_string(),
        r#"<SegmentList><SegmentURL mediaRange="é-5"/></SegmentList>"#.to_string(),
        r#"<BaseURL>http://[::1</BaseURL>"#.to_string(),
    ];
    for c in &cases {
        if let Ok(m) = dash::parse_mpd(&mpd(c), &b) {
            let _ = m.plan(Quality::Best);
        }
    }
    // A long r=-1 timeline is bounded, not unbounded memory.
    let long = mpd(
        r#"<SegmentTemplate media="$Number$" timescale="1000000"><SegmentTimeline><S t="0" d="1" r="-1"/></SegmentTimeline></SegmentTemplate>"#,
    );
    assert!(dash::parse_mpd(&long, &b).is_err());
}
