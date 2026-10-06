use futures::{future::{select_ok, BoxFuture}, FutureExt};
use regex::Regex;
use reqwest::{redirect::Policy, Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Instant;
use tokio::time::{timeout, Duration};
use url::Url;
use vercel_runtime::{run, Error, Request};

const MAX_BODY: usize = 6 * 1024 * 1024;
const MAX_HTML: usize = 4 * 1024 * 1024;
const MAX_TRANSCRIPT_CHARS: usize = 1_200_000;
const MAX_SEGMENTS: usize = 80_000;
const MAX_RESPONSE_BYTES: usize = 4_000_000;
const ATTEMPT_TIMEOUT_SECS: u64 = 28;
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36 ArixYT/1.0";

#[derive(Debug, Clone)]
struct AttemptError {
    label: &'static str,
    message: String,
}

type AttemptResult = Result<Extraction, AttemptError>;

#[derive(Debug, Clone)]
struct Extraction {
    video: VideoInfo,
    language: LanguageInfo,
    segments: Vec<Segment>,
    method: String,
    acquisition_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
struct VideoInfo {
    id: String,
    url: String,
    title: String,
    channel: String,
    #[serde(rename = "durationSeconds")]
    duration_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct LanguageInfo {
    code: String,
    name: String,
    generated: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Segment {
    text: String,
    start: f64,
    duration: f64,
}

#[derive(Debug, Deserialize)]
struct Input {
    url: String,
    lang: Option<String>,
}

#[derive(Debug)]
struct CaptionTrack {
    base_url: String,
    language_code: String,
    language_name: String,
    generated: bool,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    run(handler).await
}

async fn handler(req: Request) -> Result<Value, Error> {
    if req.method().as_str() != "POST" {
        return Ok(json!({"ok": false, "error": {"code": "METHOD_NOT_ALLOWED", "message": "Use POST."}}));
    }
    if req.body().len() > 16 * 1024 {
        return Ok(error_json("REQUEST_TOO_LARGE", "Request body is limited to 16 KB."));
    }
    let input: Input = match serde_json::from_slice(req.body()) {
        Ok(v) => v,
        Err(_) => return Ok(error_json("INVALID_JSON", "Request body must be JSON like {\"url\":\"https://youtu.be/...\"}.")),
    };
    let id = match extract_video_id(&input.url) {
        Ok(v) => v,
        Err(msg) => return Ok(error_json("INVALID_YOUTUBE_URL", &msg)),
    };

    let overall = Instant::now();
    let client = match make_client() {
        Ok(v) => v,
        Err(e) => return Ok(error_json("CLIENT_INIT_FAILED", &e.to_string())),
    };
    let lang = input.lang.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let mut attempts: Vec<BoxFuture<'static, AttemptResult>> = Vec::with_capacity(4);
    attempts.push(watch_attempt(client.clone(), id.clone(), lang.map(str::to_string)).boxed());
    attempts.push(inner_tube_attempt(client.clone(), id.clone(), lang.map(str::to_string), "ANDROID").boxed());
    attempts.push(inner_tube_attempt(client.clone(), id.clone(), lang.map(str::to_string), "IOS").boxed());
    attempts.push(inner_tube_attempt(client.clone(), id.clone(), lang.map(str::to_string), "TVHTML5").boxed());

    let joined = select_ok(attempts);
    let extraction: AttemptResult = match timeout(Duration::from_secs(24), joined).await {
        Ok(Ok((value, _remaining))) => Ok(value),
        Ok(Err(errors)) => Err(errors.into_iter().next().unwrap_or(AttemptError { label: "all", message: "No transcript path succeeded." })),
        Err(_) => Err(AttemptError { label: "all", message: "Timed out before a public transcript path succeeded." }),
    };

    match extraction {
        Ok(out) => {
            let server_ms = overall.elapsed().as_secs_f64() * 1000.0;
            let text = out.segments.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" ");
            let word_count = text.split_whitespace().count();
            let segment_count = out.segments.len();
            let payload = json!({
                "ok": true,
                "video": out.video,
                "language": out.language,
                "transcript": {"text": text, "segments": out.segments},
                "meta": {
                    "version": "arix-youtube-transcript-1.0.0",
                    "method": out.method,
                    "segmentCount": segment_count,
                    "wordCount": word_count,
                    "charCount": text.chars().count(),
                    "serverMs": server_ms,
                    "acquisitionMs": out.acquisition_ms
                }
            });
            let bytes = serde_json::to_vec(&payload).unwrap_or_default();
            if bytes.len() > MAX_RESPONSE_BYTES {
                return Ok(error_json("OUTPUT_TOO_LARGE", "The transcript is larger than the serverless response safety limit. Use a shorter video or a chunked API design."));
            }
            Ok(payload)
        }
        Err(e) => Ok(error_json("TRANSCRIPT_UNAVAILABLE", &format!("{}: {}", e.label, e.message))),
    }
}

fn error_json(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn make_client() -> Result<Client, reqwest::Error> {
    let policy = Policy::custom(|attempt| {
        if attempt.previous().len() >= 4 {
            return attempt.stop();
        }
        match attempt.url().host_str() {
            Some(host) if allowed_host(host) && attempt.url().scheme() == "https" => attempt.follow(),
            _ => attempt.stop(),
        }
    });
    Client::builder()
        .user_agent(USER_AGENT)
        .redirect(policy)
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        .http2_adaptive_window(true)
        .pool_idle_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(12)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .build()
}

fn allowed_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(h.as_str(), "youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be" | "www.youtube-nocookie.com" | "youtube-nocookie.com") || h.ends_with(".youtube.com")
}

fn extract_video_id(raw: &str) -> Result<String, String> {
    let candidate = raw.trim();
    if candidate.is_empty() || candidate.len() > 2048 {
        return Err("URL is empty or too long.".into());
    }
    if Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap().is_match(candidate) {
        return Ok(candidate.to_string());
    }
    let normalized = if candidate.starts_with("http://") || candidate.starts_with("https://") { candidate.to_string() } else { format!("https://{}", candidate) };
    let url = Url::parse(&normalized).map_err(|_| "Invalid URL.".to_string())?;
    if url.scheme() != "https" { return Err("Only HTTPS YouTube URLs are accepted.".into()); }
    let host = url.host_str().unwrap_or_default();
    if !allowed_host(host) { return Err("Only youtube.com and youtu.be URLs are accepted.".into()); }
    let path = url.path().trim_matches('/');
    let id = if host.ends_with("youtu.be") {
        path.split('/').next().unwrap_or("")
    } else if path == "watch" {
        url.query_pairs().find_map(|(k,v)| if k == "v" { Some(v.into_owned()) } else { None }).unwrap_or_default().as_str().to_string()
    } else if path.starts_with("shorts/") || path.starts_with("embed/") || path.starts_with("live/") {
        path.split('/').nth(1).unwrap_or("").to_string()
    } else {
        url.query_pairs().find_map(|(k,v)| if k == "v" { Some(v.into_owned()) } else { None }).unwrap_or_default()
    };
    if Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap().is_match(&id) { Ok(id) } else { Err("Could not find a valid 11-character YouTube video ID.".into()) }
}

async fn watch_attempt(client: Client, id: String, lang: Option<String>) -> AttemptResult {
    match timeout(Duration::from_secs(ATTEMPT_TIMEOUT_SECS), watch_attempt_inner(client, id.clone(), lang)).await {
        Ok(v) => v,
        Err(_) => Err(AttemptError { label: "watch", message: "watch-page path timed out.".into() }),
    }
}

async fn watch_attempt_inner(client: Client, id: String, lang: Option<String>) -> AttemptResult {
    let started = Instant::now();
    let watch_url = format!("https://www.youtube.com/watch?v={id}");
    let html = fetch_text(&client, &watch_url, MAX_HTML).await.map_err(|e| ae("watch", e))?;
    if looks_like_challenge(&html) { return Err(ae("watch", "YouTube returned a verification or challenge page.")); }
    let player = extract_json_object_after_key(&html, "ytInitialPlayerResponse").ok_or_else(|| ae("watch", "Player metadata was not present."))?;
    let video = parse_video(&player, &id, &watch_url);
    assert_playable(&player)?;
    let tracks = extract_tracks(&player);
    if tracks.is_empty() {
        if let Some(track) = fetch_searchable_transcript(&client, &html, &id, lang.as_deref()).await.map_err(|e| ae("watch-search", e))? {
            return Ok(Extraction { video, language: track.0, segments: track.1, method: "watch-next-get_transcript".into(), acquisition_ms: started.elapsed().as_secs_f64()*1000.0 });
        }
        return Err(ae("watch", "No caption tracks were advertised by YouTube."));
    }
    let selected = choose_track(&tracks, lang.as_deref());
    let direct_error = match fetch_track_any_format(&client, selected).await {
        Ok((segments, fmt)) => {
            let segments = finalize_segments(segments).map_err(|e| ae("watch-caption", e))?;
            return Ok(Extraction { video, language: LanguageInfo { code: selected.language_code.clone(), name: selected.language_name.clone(), generated: selected.generated }, segments, method: format!("watch-{fmt}"), acquisition_ms: started.elapsed().as_secs_f64()*1000.0 });
        }
        Err(e) => e,
    };
    if let Some(track) = fetch_searchable_transcript(&client, &html, &id, lang.as_deref()).await.map_err(|e| ae("watch-search", e))? {
        return Ok(Extraction { video, language: track.0, segments: track.1, method: "watch-next-get_transcript".into(), acquisition_ms: started.elapsed().as_secs_f64()*1000.0 });
    }
    Err(ae("watch-caption", format!("Direct captions failed: {direct_error}")))
}

async fn inner_tube_attempt(client: Client, id: String, lang: Option<String>, client_name: &'static str) -> AttemptResult {
    let started = Instant::now();
    let result = timeout(Duration::from_secs(ATTEMPT_TIMEOUT_SECS), inner_tube_attempt_inner(client, id.clone(), lang, client_name)).await;
    match result {
        Ok(v) => v,
        Err(_) => Err(AttemptError { label: client_name, message: "InnerTube player path timed out.".into() }),
    }.map(|mut out| { out.acquisition_ms = started.elapsed().as_secs_f64()*1000.0; out })
}

async fn inner_tube_attempt_inner(client: Client, id: String, lang: Option<String>, client_name: &'static str) -> AttemptResult {
    let (version, platform, client_version) = match client_name {
        "ANDROID" => ("ANDROID", "MOBILE", "20.10.38"),
        "IOS" => ("IOS", "MOBILE", "21.26.4"),
        _ => ("TVHTML5", "TV", "7.20260707.07.00"),
    };
    let endpoint = "https://www.youtube.com/youtubei/v1/player";
    let body = json!({"videoId": id, "contentCheckOk": true, "racyCheckOk": true, "context": {"client": {"clientName": version, "clientVersion": client_version, "platform": platform, "hl": "en", "gl": "US"}}});
    let value = post_json(&client, endpoint, body).await.map_err(|e| ae(client_name, e))?;
    assert_playable(&value)?;
    let video = parse_video(&value, &id, &format!("https://www.youtube.com/watch?v={id}"));
    let tracks = extract_tracks(&value);
    if tracks.is_empty() { return Err(ae(client_name, "No caption tracks in player response.")); }
    let selected = choose_track(&tracks, lang.as_deref());
    let (segments, fmt) = fetch_track_any_format(&client, &selected).await.map_err(|e| ae(client_name, e))?;
    let segments = finalize_segments(segments).map_err(|e| ae(client_name, e))?;
    Ok(Extraction { video, language: LanguageInfo { code: selected.language_code, name: selected.language_name, generated: selected.generated }, segments, method: format!("innertube-{client_name}-{fmt}"), acquisition_ms: 0.0 })
}

fn ae(label: &'static str, message: impl Into<String>) -> AttemptError { AttemptError { label, message: message.into() } }

async fn fetch_text(client: &Client, url: &str, max: usize) -> Result<String, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    read_limited(response, max).await.map_err(|e| e.to_string()).and_then(|b| String::from_utf8(b).map_err(|_| "Upstream response was not UTF-8.".into()))
}

async fn post_json(client: &Client, url: &str, body: Value) -> Result<Value, String> {
    let response = client.post(url).header("content-type", "application/json").json(&body).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err(format!("YouTube returned HTTP {}.", response.status())); }
    let body = read_limited(response, MAX_BODY).await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|_| "YouTube returned invalid JSON.".into())
}

async fn read_limited(response: Response, max: usize) -> Result<Vec<u8>, std::io::Error> {
    if let Some(len) = response.content_length() { if len as usize > max { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Upstream response exceeded safety limit.")); } }
    let body = response.bytes().await.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    if body.len() > max { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Upstream response exceeded safety limit.")); }
    Ok(body.to_vec())
}

fn looks_like_challenge(html: &str) -> bool {
    let h = html.to_ascii_lowercase();
    ["captcha", "verify you're a human", "verify you’re a human", "unusual traffic", "sorry for the interruption", "sign in to confirm you're not a bot"].iter().any(|x| h.contains(x))
}

fn extract_json_object_after_key(input: &str, key: &str) -> Option<Value> {
    let needles = [format!("var {key} = "), format!("{key} = "), format!("\"{key}\":")];
    let start = needles.iter().filter_map(|n| input.find(n).map(|i| i+n.len())).min()?;
    let bytes = input.as_bytes(); let mut i = start; while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
    while i < bytes.len() && bytes[i] != b'{' { i += 1; }
    if i >= bytes.len() { return None; }
    let begin = i; let mut depth = 0usize; let mut in_str = false; let mut esc = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str { if esc { esc = false; } else if c == b'\\' { esc = true; } else if c == b'"' { in_str = false; } }
        else { if c == b'"' { in_str = true; } else if c == b'{' { depth += 1; } else if c == b'}' { depth -= 1; if depth == 0 { return serde_json::from_slice(&bytes[begin..=i]).ok(); } } }
        i += 1;
    }
    None
}

fn parse_video(player: &Value, id: &str, url: &str) -> VideoInfo {
    let title = player.pointer("/videoDetails/title").and_then(Value::as_str).unwrap_or("").to_string();
    let channel = player.pointer("/videoDetails/author").and_then(Value::as_str).unwrap_or("").to_string();
    let duration_seconds = player.pointer("/videoDetails/lengthSeconds").and_then(Value::as_str).and_then(|s| s.parse().ok());
    VideoInfo { id: id.to_string(), url: url.to_string(), title, channel, duration_seconds }
}

fn assert_playable(player: &Value) -> Result<(), AttemptError> {
    let status = player.pointer("/playabilityStatus/status").and_then(Value::as_str).unwrap_or("OK");
    if status == "OK" { return Ok(()); }
    let reason = player.pointer("/playabilityStatus/reason").and_then(Value::as_str).unwrap_or("Video is not currently playable through this public YouTube path.").to_string();
    Err(ae("youtube", reason))
}

fn extract_tracks(player: &Value) -> Vec<CaptionTrack> {
    let tracks = player.pointer("/captions/playerCaptionsTracklistRenderer/captionTracks").and_then(Value::as_array).cloned().unwrap_or_default();
    tracks.into_iter().filter_map(|t| {
        let base_url = t.get("baseUrl").and_then(Value::as_str)?.to_string();
        let language_code = t.get("languageCode").and_then(Value::as_str).unwrap_or("und").to_string();
        let language_name = t.pointer("/name/simpleText").and_then(Value::as_str).or_else(|| t.pointer("/name/runs/0/text").and_then(Value::as_str)).unwrap_or(&language_code).to_string();
        let generated = t.get("kind").and_then(Value::as_str) == Some("asr");
        Some(CaptionTrack { base_url, language_code, language_name, generated })
    }).collect()
}

fn choose_track<'a>(tracks: &'a [CaptionTrack], requested: Option<&str>) -> &'a CaptionTrack {
    if let Some(req) = requested {
        if let Some(t) = tracks.iter().find(|t| t.language_code.eq_ignore_ascii_case(req) && !t.generated) { return t; }
        if let Some(t) = tracks.iter().find(|t| t.language_code.eq_ignore_ascii_case(req)) { return t; }
        let prefix = req.to_ascii_lowercase();
        if let Some(t) = tracks.iter().find(|t| t.language_code.to_ascii_lowercase().starts_with(&prefix) && !t.generated) { return t; }
    }
    tracks.iter().find(|t| !t.generated).unwrap_or(&tracks[0])
}

async fn fetch_track_any_format(client: &Client, track: &CaptionTrack) -> Result<(Vec<Segment>, &'static str), String> {
    let formats = ["json3", "vtt", "srv1"];
    let mut jobs: Vec<BoxFuture<'static, Result<(Vec<Segment>, &'static str), String>>> = Vec::with_capacity(formats.len());
    for fmt in formats { jobs.push(fetch_one_format(client.clone(), track.base_url.clone(), fmt).boxed()); }
    select_ok(jobs).await.map(|(v, _)| v).map_err(|errs| errs.into_iter().map(|e| e.to_string()).collect::<Vec<_>>().join(" | "))
}

async fn fetch_one_format(client: Client, base_url: String, fmt: &'static str) -> Result<(Vec<Segment>, &'static str), String> {
    let mut url = Url::parse(&base_url).map_err(|_| "Invalid caption URL.".to_string())?;
    if url.scheme() != "https" || !allowed_host(url.host_str().unwrap_or_default()) {
        return Err("Caption URL failed the YouTube host safety check.".into());
    }
    url.query_pairs_mut().append_pair("fmt", fmt);
    let response = client.get(url).header("accept", "*/*").send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err(format!("{fmt}: HTTP {}", response.status())); }
    let body = read_limited(response, MAX_BODY).await.map_err(|e| e.to_string())?;
    if body.is_empty() { return Err(format!("{fmt}: empty response")); }
    let segs = match fmt {
        "json3" => parse_json3(&body),
        "vtt" => String::from_utf8(body).ok().and_then(|s| parse_vtt(&s)),
        _ => String::from_utf8(body).ok().and_then(|s| parse_srv1(&s)),
    }.ok_or_else(|| format!("{fmt}: parse failed"))?;
    if segs.is_empty() { return Err(format!("{fmt}: no segments")); }
    Ok((segs, fmt))
}

fn parse_json3(body: &[u8]) -> Option<Vec<Segment>> {
    let v: Value = serde_json::from_slice(body).ok()?; let mut out = Vec::new();
    for ev in v.get("events")?.as_array()? {
        let start = ev.get("tStartMs").and_then(Value::as_i64).unwrap_or(0) as f64 / 1000.0;
        let duration = ev.get("dDurationMs").and_then(Value::as_i64).unwrap_or(0) as f64 / 1000.0;
        let text = ev.get("segs").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.get("utf8").and_then(Value::as_str)).collect::<String>()).unwrap_or_default();
        let text = clean_text(&text); if !text.is_empty() { out.push(Segment { text, start, duration }); }
    }
    Some(out)
}

fn parse_vtt(s: &str) -> Option<Vec<Segment>> {
    let lines: Vec<&str> = s.lines().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if let Some((a, b)) = parse_vtt_time_line(lines[i]) {
            i += 1;
            let mut parts = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                if parse_vtt_time_line(lines[i]).is_some() { break; }
                parts.push(lines[i].trim());
                i += 1;
            }
            let text = clean_text(&parts.join(" "));
            if !text.is_empty() { out.push(Segment { text, start: a, duration: (b-a).max(0.0) }); }
        } else {
            i += 1;
        }
    }
    Some(out)
}

fn parse_vtt_time_line(line: &str) -> Option<(f64,f64)> {
    let (a,b) = line.split_once(" --> ")?; Some((parse_time(a)?, parse_time(b.split_whitespace().next()?)?))
}

fn parse_time(s: &str) -> Option<f64> {
    let normalized = s.trim().replace(',', ".");
    let p: Vec<&str> = normalized.split(':').collect();
    if p.len() != 3 { return None; }
    Some(p[0].parse::<f64>().ok()?*3600.0 + p[1].parse::<f64>().ok()?*60.0 + p[2].parse::<f64>().ok()?)
}

fn parse_srv1(s: &str) -> Option<Vec<Segment>> {
    let tag_re = Regex::new(r#"(?s)<text([^>]*)>(.*?)</text>"#).ok()?;
    let attr_re = Regex::new(r#"\b(start|dur)=\"([^\"]*)\""#).ok()?;
    Some(tag_re.captures_iter(s).filter_map(|c| {
        let attrs = c.get(1)?.as_str();
        let mut start = None; let mut duration = None;
        for a in attr_re.captures_iter(attrs) {
            match a.get(1)?.as_str() {
                "start" => start = a.get(2).and_then(|x| x.as_str().parse::<f64>().ok()),
                "dur" => duration = a.get(2).and_then(|x| x.as_str().parse::<f64>().ok()),
                _ => {}
            }
        }
        let text = clean_text(&decode_xml_entities(c.get(2)?.as_str()));
        match (start, duration) {
            (Some(start), Some(duration)) if !text.is_empty() => Some(Segment { text, start, duration }),
            _ => None
        }
    }).collect())
}

fn clean_text(s: &str) -> String { s.replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ") }

fn decode_xml_entities(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&#39;", "'").replace("&quot;", "\"").replace("&#x27;", "'")
}

fn finalize_segments(mut segments: Vec<Segment>) -> Result<Vec<Segment>, String> {
    if segments.is_empty() { return Err("Transcript contained no usable text.".into()); }
    segments.sort_by(|a,b| a.start.total_cmp(&b.start));
    segments.dedup_by(|a,b| a.start == b.start && a.text == b.text);
    for i in 0..segments.len() { if segments[i].duration <= 0.0 { segments[i].duration = if i+1 < segments.len() { (segments[i+1].start - segments[i].start).max(0.1) } else { 2.0 }; } }
    if segments.len() > MAX_SEGMENTS { segments.truncate(MAX_SEGMENTS); }
    let chars: usize = segments.iter().map(|s| s.text.chars().count()).sum::<usize>();
    if chars > MAX_TRANSCRIPT_CHARS { return Err("Transcript exceeded the safety size limit.".into()); }
    Ok(segments)
}

fn to_srt(segments: &[Segment]) -> String {
    segments.iter().enumerate().map(|(i,s)| format!("{}\n{} --> {}\n{}\n", i+1, srt_time(s.start), srt_time(s.start + s.duration), s.text)).collect::<Vec<_>>().join("\n")
}

fn srt_time(x: f64) -> String {
    let ms = (x.max(0.0)*1000.0).round() as u64; let h = ms/3_600_000; let m = (ms%3_600_000)/60_000; let s=(ms%60_000)/1000; let z=ms%1000; format!("{h:02}:{m:02}:{s:02},{z:03}")
}

async fn fetch_searchable_transcript(client: &Client, html: &str, id: &str, lang: Option<&str>) -> Result<Option<(LanguageInfo, Vec<Segment>)>, String> {
    let api_key = extract_config_string(html, "INNERTUBE_API_KEY"); let client_version = extract_config_string(html, "INNERTUBE_CLIENT_VERSION");
    let (Some(key), Some(version)) = (api_key, client_version) else { return Ok(None); };
    let context = json!({"client":{"clientName":"WEB","clientVersion":version,"hl":"en","gl":"US"}});
    let next_url = format!("https://www.youtube.com/youtubei/v1/next?key={key}");
    let next = post_json(client, &next_url, json!({"videoId":id,"context":context.clone()})).await?;
    let Some(params) = find_transcript_params(&next) else { return Ok(None); };
    let get_url = format!("https://www.youtube.com/youtubei/v1/get_transcript?key={key}");
    let value = post_json(client, &get_url, json!({"context":context,"params":params})).await?;
    let segments = parse_searchable_transcript(&value).ok_or_else(|| "YouTube transcript renderer could not be parsed.".to_string())?;
    if segments.is_empty() { return Ok(None); }
    let segments = finalize_segments(segments)?;
    let code = lang.unwrap_or("en").to_string();
    Ok(Some((LanguageInfo { code, name: "YouTube transcript".into(), generated: false }, segments)))
}

fn extract_config_string(html: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\""); let start = html.find(&needle)? + needle.len(); let rest = &html[start..]; let end = rest.find('"')?; Some(rest[..end].replace("\\u0026", "&"))
}

fn find_transcript_params(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            if let Some(endpoint) = map.get("getTranscriptEndpoint") {
                if let Some(params) = endpoint.get("params").and_then(Value::as_str) { return Some(params.to_string()); }
            }
            for child in map.values() { if let Some(x) = find_transcript_params(child) { return Some(x); } }
        }
        Value::Array(arr) => for child in arr { if let Some(x) = find_transcript_params(child) { return Some(x); } },
        _ => {}
    }
    None
}

fn parse_searchable_transcript(v: &Value) -> Option<Vec<Segment>> {
    let mut out = Vec::new();
    collect_transcript_runs(v, &mut out, 0.0);
    Some(out)
}

fn collect_transcript_runs(v: &Value, out: &mut Vec<Segment>, mut cursor: f64) {
    match v {
        Value::Object(map) => {
            if let Some(renderer) = map.get("transcriptSegmentRenderer") {
                let start = renderer.get("startMs").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()).unwrap_or(cursor*1000.0)/1000.0;
                let end = renderer.get("endMs").and_then(Value::as_str).and_then(|s| s.parse::<f64>().ok()).unwrap_or((start+2.0)*1000.0)/1000.0;
                let text = renderer.pointer("/snippet/runs").and_then(Value::as_array).map(|runs| runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)).collect::<String>()).unwrap_or_default();
                let text = clean_text(&text); if !text.is_empty() { out.push(Segment { text, start, duration:(end-start).max(0.1) }); }
                cursor = end;
            }
            for child in map.values() { collect_transcript_runs(child,out,cursor); }
        }
        Value::Array(arr) => for child in arr { collect_transcript_runs(child,out,cursor) },
        _ => {}
    }
}
