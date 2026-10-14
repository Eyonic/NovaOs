//! Shields: Nova Browser's privacy layer (a clean-room design, written for Nova).
//!
//! The Chromium engine asks Shields about every request (DevTools Fetch domain);
//! Shields answers block / upgrade / rewrite / allow:
//!
//! * **Ad and tracker blocking.** A built-in list of well-known ad, analytics and
//!   tracking hosts, plus filter lists in the common Adblock Plus syntax: the ones
//!   bundled in the image (`usr/share/nova/shields/*.txt`, EasyList and EasyPrivacy
//!   fetched at build time) and any saved under `system/shields/*.txt`. Syntax: `||host^` anchors, `*` wildcards, `^`
//!   separators, `|` anchors, `@@` exceptions and the `$third-party`,
//!   `$~third-party` and `$domain=` options. Cosmetic (`##`) rules are ignored.
//! * **HTTPS upgrades** for page loads on plain HTTP (except local addresses).
//! * **Tracking-parameter removal** (`utm_*`, `fbclid`, `gclid`, ...) from page URLs.
//! * **Per-site switch.** Sites listed in `system/shields/allowed-sites` have
//!   Shields down.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs;

/// Well-known advertising, analytics and tracking hosts (and their subdomains).
const BUILTIN_HOSTS: &[&str] = &[
    // Advertising networks and exchanges
    "doubleclick.net", "googlesyndication.com", "googleadservices.com", "adservice.google.com", "pagead2.googlesyndication.com",
    "adnxs.com", "adsrvr.org", "advertising.com", "amazon-adsystem.com", "criteo.com", "criteo.net", "taboola.com",
    "outbrain.com", "pubmatic.com", "rubiconproject.com", "openx.net", "casalemedia.com", "indexww.com", "smartadserver.com",
    "adform.net", "yieldmo.com", "sharethrough.com", "media.net", "33across.com", "teads.tv", "bidswitch.net",
    "contextweb.com", "lijit.com", "sovrn.com", "triplelift.com", "gumgum.com", "adroll.com", "moatads.com",
    "serving-sys.com", "zedo.com", "revcontent.com", "mgid.com", "propellerads.com", "popads.net", "adcolony.com",
    "applovin.com", "unityads.unity3d.com", "inmobi.com", "adsafeprotected.com", "doubleverify.com", "adsymptotic.com",
    "ads.yahoo.com", "ads.twitter.com", "ads.linkedin.com", "ads-api.tiktok.com", "an.facebook.com",
    // Analytics and tracking
    "google-analytics.com", "googletagmanager.com", "googletagservices.com", "analytics.google.com",
    "scorecardresearch.com", "quantserve.com", "hotjar.com", "mixpanel.com", "segment.io", "segment.com",
    "amplitude.com", "fullstory.com", "mouseflow.com", "crazyegg.com", "luckyorange.com", "newrelic.com", "nr-data.net",
    "chartbeat.com", "chartbeat.net", "parsely.com", "kissmetrics.com", "heap.io", "heapanalytics.com", "optimizely.com",
    "clarity.ms", "bat.bing.com", "connect.facebook.net", "pixel.facebook.com", "analytics.tiktok.com", "snap.licdn.com",
    "px.ads.linkedin.com", "static.ads-twitter.com", "analytics.twitter.com", "t.co.trk", "branch.io", "app.link",
    "adjust.com", "appsflyer.com", "kochava.com", "omtrdc.net", "demdex.net", "everesttech.net", "krxd.net",
    "bluekai.com", "exelator.com", "agkn.com", "rlcdn.com", "tapad.com", "liveramp.com", "crwdcntrl.net",
    "eyeota.net", "addthis.com", "sharethis.com", "disqusads.com", "statcounter.com", "histats.com", "yandex.ru.metrika",
    "mc.yandex.ru", "top-fwz1.mail.ru", "cnzz.com", "hm.baidu.com", "matomo.cloud", "piwik.pro", "plausible.io.ext",
    // Fingerprinting and session replay
    "fingerprintjs.com", "fpjs.io", "iovation.com", "threatmetrix.com", "inspectlet.com", "smartlook.com", "logrocket.io",
];

/// URL query parameters used only for tracking.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "gclsrc", "dclid", "msclkid", "mc_eid", "mc_cid", "igshid", "yclid", "_hsenc", "_hsmi",
    "mkt_tok", "oly_anon_id", "oly_enc_id", "vero_id", "wickedid", "twclid", "ttclid", "srsltid", "si",
];

#[derive(Clone)]
struct Rule {
    /// Pattern after removing anchors; `*` wildcard, `^` separator.
    pattern: String,
    /// `||`: match at a host (or subdomain) start.
    host_anchor: bool,
    /// `|` at the start / end.
    start_anchor: bool,
    end_anchor: bool,
    third_party: Option<bool>,
    /// `$domain=`: only on these page hosts (empty = everywhere); `~` excluded.
    domains: Vec<String>,
    not_domains: Vec<String>,
    /// Longest literal piece of the pattern: a cheap `contains` check that has to
    /// pass before the full match runs (keeps large lists fast).
    token: String,
}

/// Rules indexed by a word every matching URL must contain (as a whole word between
/// non-alphanumeric characters), so a request is only checked against the few rules
/// whose word occurs in its URL. Rules without such a word are checked every time.
#[derive(Default)]
struct RuleSet {
    rules: Vec<Rule>,
    by_word: BTreeMap<String, Vec<u32>>,
    generic: Vec<u32>,
}

impl RuleSet {
    fn push(&mut self, r: Rule) {
        let i = self.rules.len() as u32;
        match index_word(&r) {
            Some(w) => self.by_word.entry(w).or_default().push(i),
            None => self.generic.push(i),
        }
        self.rules.push(r);
    }

    fn any_match(&self, url: &str, words: &[&str], host: &str, page_host: &str, third_party: bool) -> bool {
        let hit = |i: &u32| self.rules[*i as usize].matches(url, host, page_host, third_party);
        words.iter().any(|w| self.by_word.get(*w).is_some_and(|v| v.iter().any(hit))) || self.generic.iter().any(hit)
    }
}

/// The longest alphanumeric run of a rule's pattern that must appear in a matching URL
/// as a complete word: bounded on both sides by a literal non-alphanumeric character,
/// a `^`, an anchored pattern start or an anchored end (not by a `*`).
fn index_word(r: &Rule) -> Option<String> {
    let p = r.pattern.as_bytes();
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < p.len() {
        if !p[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < p.len() && p[i].is_ascii_alphanumeric() {
            i += 1;
        }
        let left_ok = if start == 0 { r.host_anchor || r.start_anchor } else { p[start - 1] != b'*' };
        let right_ok = if i == p.len() { r.end_anchor } else { p[i] != b'*' };
        if left_ok && right_ok && i - start >= 2 && best.is_none_or(|(a, b)| i - start > b - a) {
            best = Some((start, i));
        }
    }
    best.map(|(a, b)| r.pattern[a..b].into())
}

pub struct Shields {
    hosts: BTreeSet<String>,
    rules: RuleSet,
    exceptions: RuleSet,
    allowed_sites: BTreeSet<String>,
    pub list_rules: usize,
}

pub enum Verdict {
    Allow,
    Block(String),
    /// Continue, but at this URL instead (HTTPS upgrade, parameters removed).
    Rewrite(String),
}

pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    authority.split(':').next().unwrap_or("")
}

/// The registrable part used to decide first vs third party (last two labels;
/// good enough without the public-suffix list).
fn site_of(host: &str) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 { host.into() } else { labels[labels.len() - 2..].join(".") }
}

/// `a.b.example.com`, `b.example.com`, `example.com`, `com`.
fn suffixes(host: &str) -> impl Iterator<Item = &str> {
    core::iter::once(host).chain(host.match_indices('.').map(move |(i, _)| &host[i + 1..]))
}

fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Match `pattern` (with `*` and `^`) against `text` starting at `t`.
fn glob(p: &[u8], text: &[u8], end_anchor: bool) -> bool {
    fn sep(c: u8) -> bool {
        !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'%'))
    }
    fn go(p: &[u8], t: &[u8], end_anchor: bool) -> bool {
        match p.first() {
            None => !end_anchor || t.is_empty(),
            Some(b'*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..], end_anchor)),
            Some(b'^') => match t.first() {
                None => go(&p[1..], t, end_anchor),
                Some(&c) if sep(c) => go(&p[1..], &t[1..], end_anchor),
                _ => false,
            },
            Some(&c) => t.first().is_some_and(|&d| d.eq_ignore_ascii_case(&c)) && go(&p[1..], &t[1..], end_anchor),
        }
    }
    go(p, text, end_anchor)
}

impl Rule {
    fn parse(line: &str) -> Option<Rule> {
        let (body, opts) = match line.rsplit_once('$') {
            Some((b, o)) if !o.contains('/') => (b, o),
            _ => (line, ""),
        };
        let mut r = Rule { pattern: String::new(), host_anchor: false, start_anchor: false, end_anchor: false, third_party: None, domains: Vec::new(), not_domains: Vec::new(), token: String::new() };
        for o in opts.split(',').filter(|o| !o.is_empty()) {
            match o {
                "third-party" | "3p" => r.third_party = Some(true),
                "~third-party" | "1p" | "first-party" => r.third_party = Some(false),
                o if o.starts_with("domain=") => {
                    for d in o[7..].split('|') {
                        match d.strip_prefix('~') {
                            Some(x) => r.not_domains.push(x.into()),
                            None => r.domains.push(d.into()),
                        }
                    }
                }
                // Options that change behaviour in ways we do not model: skip the rule.
                o if o.starts_with("redirect") || o.starts_with("csp") || o.starts_with("removeparam") || o == "badfilter" || o == "generichide" || o == "elemhide" || o == "document" || o == "popup" => return None,
                _ => {}
            }
        }
        let mut b = body;
        if let Some(x) = b.strip_prefix("||") {
            r.host_anchor = true;
            b = x;
        } else if let Some(x) = b.strip_prefix('|') {
            r.start_anchor = true;
            b = x;
        }
        if let Some(x) = b.strip_suffix('|') {
            r.end_anchor = true;
            b = x;
        }
        if b.is_empty() || b == "*" || (b.starts_with('/') && b.ends_with('/') && b.len() > 2) {
            return None; // empty, match-all, or regular expressions (not supported)
        }
        r.pattern = b.to_ascii_lowercase();
        r.token = r.pattern.split(['*', '^']).max_by_key(|t| t.len()).unwrap_or("").into();
        Some(r)
    }

    fn matches(&self, url: &str, host: &str, page_host: &str, third_party: bool) -> bool {
        if !self.token.is_empty() && !url.contains(self.token.as_str()) {
            return false;
        }
        if self.third_party.is_some_and(|t| t != third_party) {
            return false;
        }
        if !self.domains.is_empty() && !self.domains.iter().any(|d| host_matches(page_host, d)) {
            return false;
        }
        if self.not_domains.iter().any(|d| host_matches(page_host, d)) {
            return false;
        }
        let p = self.pattern.as_bytes();
        let u = url.as_bytes();
        if self.host_anchor {
            // Try the pattern at the start of the host and of every subdomain boundary.
            let Some(host_start) = url.find(host) else { return false };
            let mut starts = alloc::vec![host_start];
            for (i, c) in host.char_indices() {
                if c == '.' {
                    starts.push(host_start + i + 1);
                }
            }
            return starts.into_iter().any(|s| glob(p, &u[s..], self.end_anchor));
        }
        if self.start_anchor {
            return glob(p, u, self.end_anchor);
        }
        (0..u.len()).any(|i| glob(p, &u[i..], self.end_anchor))
    }
}

impl Shields {
    pub fn load() -> Shields {
        let mut s = Shields { hosts: BUILTIN_HOSTS.iter().map(|h| h.to_string()).collect(), rules: RuleSet::default(), exceptions: RuleSet::default(), allowed_sites: BTreeSet::new(), list_rules: 0 };
        for prefix in ["usr/share/nova/shields/", "system/shields/"] {
            for (name, _, _) in fs::list(prefix) {
                if name.ends_with(".txt")
                    && let Ok(text) = fs::read_string(&name)
                {
                    for line in text.lines() {
                        s.add_line(line.trim());
                    }
                }
            }
        }
        if let Ok(text) = fs::read_string("system/shields/allowed-sites") {
            s.allowed_sites = text.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
        }
        s
    }

    fn add_line(&mut self, line: &str) {
        if line.is_empty() || line.starts_with('!') || line.starts_with('[') || line.contains("##") || line.contains("#@#") || line.contains("#?#") || line.contains("#$#") {
            return;
        }
        let (exception, rest) = match line.strip_prefix("@@") {
            Some(r) => (true, r),
            None => (false, line),
        };
        // Fast path: plain "||host^" rules go into the host set.
        if !exception
            && let Some(h) = rest.strip_prefix("||").and_then(|r| r.strip_suffix('^'))
            && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            self.hosts.insert(h.to_ascii_lowercase());
            self.list_rules += 1;
            return;
        }
        if let Some(r) = Rule::parse(rest) {
            self.list_rules += 1;
            if exception { self.exceptions.push(r) } else { self.rules.push(r) }
        }
    }

    pub fn site_allowed(&self, page_host: &str) -> bool {
        self.allowed_sites.iter().any(|s| host_matches(page_host, s))
    }

    /// Toggle Shields for a site; returns true if Shields are now up there.
    pub fn toggle_site(&mut self, page_host: &str) -> bool {
        let site = site_of(page_host);
        let up = if self.allowed_sites.remove(&site) {
            true
        } else {
            self.allowed_sites.insert(site);
            false
        };
        let text: String = self.allowed_sites.iter().map(|s| format!("{s}\n")).collect();
        let _ = fs::write("system/shields/allowed-sites", text.as_bytes());
        up
    }

    /// Decide about one request. `page_url` is the top-level page (empty for the page itself).
    pub fn check(&self, url: &str, page_url: &str, is_document: bool) -> Verdict {
        let host = host_of(url).to_ascii_lowercase();
        let page_host = if page_url.is_empty() { host.clone() } else { host_of(page_url).to_ascii_lowercase() };
        if self.site_allowed(&page_host) {
            return Verdict::Allow;
        }
        if is_document {
            // Page loads: upgrade to HTTPS and drop tracking parameters.
            let mut new = String::from(url);
            if let Some(rest) = url.strip_prefix("http://")
                && !is_local(&host)
            {
                new = format!("https://{rest}");
            }
            if let Some(clean) = strip_tracking(&new) {
                new = clean;
            }
            if new != url {
                return Verdict::Rewrite(new);
            }
            return Verdict::Allow;
        }
        let third_party = site_of(&host) != site_of(&page_host);
        let lower = url.to_ascii_lowercase();
        let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 2).collect();
        if self.exceptions.any_match(&lower, &words, &host, &page_host, third_party) {
            return Verdict::Allow;
        }
        // The host or any parent domain in the host list (`||host^` rules).
        if third_party && suffixes(&host).any(|d| self.hosts.contains(d)) {
            return Verdict::Block(host);
        }
        if self.rules.any_match(&lower, &words, &host, &page_host, third_party) {
            return Verdict::Block(host);
        }
        Verdict::Allow
    }
}

fn is_local(host: &str) -> bool {
    host == "localhost" || host.ends_with(".local") || host.starts_with("10.") || host.starts_with("192.168.") || host.starts_with("127.") || host.parse::<smoltcp::wire::Ipv4Address>().is_ok()
}

/// The URL without known tracking parameters, if any were present.
pub fn strip_tracking(url: &str) -> Option<String> {
    let (base, query) = url.split_once('?')?;
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            let k = kv.split('=').next().unwrap_or("");
            !(k.starts_with("utm_") || TRACKING_PARAMS.contains(&k))
        })
        .collect();
    if kept.len() == query.split('&').count() {
        return None;
    }
    let mut out = String::from(base);
    if !kept.is_empty() {
        out.push('?');
        out.push_str(&kept.join("&"));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}
