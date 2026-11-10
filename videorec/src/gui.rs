//! `videorec gui`: a small local web app (127.0.0.1 only) for the four steps
//! source, scan, results, export. Scans and exports run this same program as
//! a child process (`recover` / `export`), so the GUI does exactly what the
//! command line does. Every API call needs the random token from the start
//! URL in an `X-Token` header, so other web pages cannot drive it.

use crate::io::{FsKind, Source};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

const PAGE: &str = include_str!("gui.html");

#[derive(Default)]
struct Job {
    child: Option<Child>,
    kind: String,
    args: Vec<String>,
    lines: Vec<String>,
    progress: String,
    exit: Option<i32>,
    started: Option<std::time::Instant>,
    took: Option<u64>,
}

type Shared = Arc<Mutex<Job>>;

pub fn run(port: u16, open_browser: bool) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
    let port = listener.local_addr()?.port();
    let token = token();
    let url = format!("http://127.0.0.1:{port}/?t={token}");
    println!("videorec GUI: {url}");
    println!("Close this window to stop.");
    if open_browser {
        open_url(&url);
    }
    let job: Shared = Arc::new(Mutex::new(Job::default()));
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let (job, token) = (job.clone(), token.clone());
        std::thread::spawn(move || {
            if let Err(e) = handle(s, &job, &token) {
                eprintln!("gui: {e}");
            }
        });
    }
    Ok(())
}

fn token() -> String {
    use sha2::Digest;
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        f.read_exact(&mut b).ok();
    }
    // Windows has no /dev/urandom: also mix in time, pid and a stack address.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = sha2::Sha256::new();
    h.update(b);
    h.update(t.to_le_bytes());
    h.update((&b as *const _ as usize).to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.finalize()[..16]
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let r = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let r = Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        println!("Open the address above in a browser.");
    }
}

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    token: String,
    range: Option<String>,
    body: Vec<u8>,
}

impl Req {
    fn q(&self, k: &str) -> String {
        self.query
            .iter()
            .find(|(a, _)| a == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_req(s: &mut TcpStream) -> Result<Option<Req>> {
    let mut r = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let target = it.next().unwrap_or("/").to_string();
    let (mut len, mut token, mut range) = (0usize, String::new(), None);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                len = v.trim().parse().unwrap_or(0);
            } else if k == "x-token" {
                token = v.trim().to_string();
            } else if k == "range" {
                range = Some(v.trim().to_string());
            }
        }
    }
    anyhow::ensure!(len <= 4 << 20, "request too large");
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect();
    Ok(Some(Req {
        method,
        path: path.to_string(),
        query,
        token,
        range,
        body,
    }))
}

fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) -> Result<()> {
    let reason = match code {
        200 => "OK",
        206 => "Partial Content",
        416 => "Range Not Satisfiable",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    write!(
        s,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    s.write_all(body)?;
    Ok(())
}

fn handle(mut s: TcpStream, job: &Shared, token: &str) -> Result<()> {
    let Some(req) = read_req(&mut s)? else {
        return Ok(());
    };
    if req.method == "GET" && req.path == "/" {
        // The page reads the token from its own URL.
        return respond(&mut s, 200, "text/html; charset=utf-8", PAGE.as_bytes());
    }
    // Media elements cannot send headers, so media URLs carry the token.
    if req.path.starts_with("/media/") {
        if req.q("t") != token {
            return respond(&mut s, 403, "text/plain", b"bad token");
        }
        let file = if req.path == "/media/out" {
            out_file(&req.q("out"), &req.q("f"))
        } else {
            cache_file(&req.path["/media/cache/".len().min(req.path.len())..])
        };
        return match file {
            Ok(p) => serve_file(&mut s, &p, req.range.as_deref()),
            Err(e) => respond(&mut s, 404, "text/plain", format!("{e:#}").as_bytes()),
        };
    }
    if !req.path.starts_with("/api/") {
        return respond(&mut s, 404, "text/plain", b"not found");
    }
    if req.token != token {
        return respond(&mut s, 403, "text/plain", b"bad token");
    }
    if req.path == "/api/thumb" {
        let r = out_file(&req.q("out"), &req.q("f")).and_then(|p| crate::preview::thumb(&p));
        return match r {
            Ok(crate::preview::Thumb::Mp4(p)) => {
                let b = std::fs::read(p)?;
                respond(&mut s, 200, "video/mp4", &b)
            }
            Ok(crate::preview::Thumb::Jpeg(b)) => respond(&mut s, 200, "image/jpeg", &b),
            Err(e) => respond(
                &mut s,
                404,
                "text/plain; charset=utf-8",
                format!("{e:#}").as_bytes(),
            ),
        };
    }
    if req.path == "/api/preview" {
        let r = out_file(&req.q("out"), &req.q("f")).and_then(|p| crate::preview::preview(&p, 120));
        let v = match r {
            Ok(p) => {
                let name = |x: &Path| x.file_name().unwrap().to_string_lossy().into_owned();
                json!({
                    "video": format!("/media/cache/{}?t={token}", name(&p.video)),
                    "audio": p.audio.as_ref().map(|a| format!("/media/cache/{}?t={token}", name(a))),
                    "seconds": p.seconds,
                    "truncated": p.truncated,
                    "skipped_leading": p.skipped_leading,
                })
            }
            Err(e) => json!({"error": format!("{e:#}")}),
        };
        return respond(&mut s, 200, "application/json", v.to_string().as_bytes());
    }
    let r = api(&req, job);
    let (code, v) = match r {
        Ok(v) => (200, v),
        Err(e) => (400, json!({"error": format!("{e:#}")})),
    };
    respond(&mut s, code, "application/json", v.to_string().as_bytes())
}

/// A recovered file: `rel` must stay inside `out`.
fn out_file(out: &str, rel: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !out.is_empty()
            && !rel.is_empty()
            && !Path::new(rel).is_absolute()
            && !rel.split(['/', '\\']).any(|c| c == ".." || c.contains(':')),
        "bad file path"
    );
    let p = Path::new(out).join(rel);
    anyhow::ensure!(p.is_file(), "{} not found", p.display());
    Ok(p)
}

fn cache_file(name: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            && !name.contains(".."),
        "bad name"
    );
    let p = crate::preview::cache_dir().join(name);
    anyhow::ensure!(p.is_file(), "not in cache");
    Ok(p)
}

/// Serves a file with HTTP Range support (needed for seeking in <video>).
fn serve_file(s: &mut TcpStream, p: &Path, range: Option<&str>) -> Result<()> {
    let mut f = std::fs::File::open(p)?;
    let size = f.metadata()?.len();
    let ctype = match p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp4" => "video/mp4",
        "wav" => "audio/wav",
        "avi" => "video/x-msvideo",
        _ => "application/octet-stream",
    };
    let (mut a, mut b) = (0u64, size.saturating_sub(1));
    let mut partial = false;
    if let Some(r) = range.and_then(|r| r.strip_prefix("bytes=")) {
        let (x, y) = r.split_once('-').unwrap_or((r, ""));
        if x.is_empty() {
            let n: u64 = y.parse().unwrap_or(0);
            a = size.saturating_sub(n);
        } else {
            a = x.parse().unwrap_or(0);
            if let Ok(e) = y.parse::<u64>() {
                b = e.min(b);
            }
        }
        partial = true;
        if a >= size || a > b {
            write!(
                s,
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )?;
            return Ok(());
        }
    }
    let len = if size == 0 { 0 } else { b - a + 1 };
    if partial {
        write!(
            s,
            "HTTP/1.1 206 Partial Content\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nContent-Range: bytes {a}-{b}/{size}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        )?;
    } else {
        write!(
            s,
            "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
        )?;
    }
    use std::io::Seek;
    f.seek(std::io::SeekFrom::Start(a))?;
    let mut left = len;
    let mut buf = vec![0u8; 1 << 20];
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        if s.write_all(&buf[..n]).is_err() {
            break; // the browser closed the connection (seek or pause)
        }
        left -= n as u64;
    }
    Ok(())
}

fn body(req: &Req) -> Result<Value> {
    Ok(serde_json::from_slice(&req.body)?)
}

fn s_of(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or("").to_string()
}

fn api(req: &Req, job: &Shared) -> Result<Value> {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/ls") => ls(&req.q("path")),
        ("GET", "/api/info") => info(Path::new(&req.q("path"))),
        ("POST", "/api/run") => start(&body(req)?, job),
        ("GET", "/api/status") => Ok(status(job)),
        ("POST", "/api/stop") => {
            // First press: ask the scan to stop and still write its report
            // (it watches for OUT/.stop). Second press: end it at once.
            let mut j = job.lock().unwrap();
            let out = j
                .args
                .iter()
                .position(|a| a == "-o")
                .and_then(|k| j.args.get(k + 1))
                .map(|o| Path::new(o).join(".stop"));
            let hard = out.as_ref().is_none_or(|p| p.exists());
            if hard {
                if let Some(c) = j.child.as_mut() {
                    c.kill().ok();
                }
            } else if let Some(p) = out {
                std::fs::write(p, b"stop")?;
            }
            Ok(json!({"ok": true, "hard": hard}))
        }
        ("GET", "/api/report") => {
            let p = Path::new(&req.q("out")).join("report.json");
            let t = std::fs::read_to_string(&p)
                .with_context(|| format!("no report at {}", p.display()))?;
            Ok(serde_json::from_str(&t)?)
        }
        ("POST", "/api/open") => {
            let b = body(req)?;
            open_path(
                Path::new(&s_of(&b, "path")),
                b["select"].as_bool().unwrap_or(false),
            )
        }
        ("POST", "/api/copy") => copy(&body(req)?),
        ("GET", "/api/space") => Ok(json!({"free": free_space(&req.q("path"))})),
        ("GET", "/api/disks") => Ok(json!({"disks": crate::io::list_disks().iter().map(|d| {
            let mut v = serde_json::to_value(d).unwrap_or_default();
            v["label"] = disk_label(d).into();
            v
        }).collect::<Vec<_>>()})),
        ("POST", "/api/mkdir") => {
            let b = body(req)?;
            let src = b["source"].as_str().unwrap_or("");
            mkdir(b["path"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""), src)
        }
        _ => anyhow::bail!("unknown call {} {}", req.method, req.path),
    }
}

fn ls(path: &str) -> Result<Value> {
    if path.is_empty() {
        #[cfg(windows)]
        {
            let mut drives: Vec<Value> = (b'A'..=b'Z')
                .map(|c| format!("{}:\\", c as char))
                .filter(|d| Path::new(d).exists())
                .map(|d| json!({"name": d, "path": d, "dir": true}))
                .collect();
            // Physical disks, numbered as in Disk Management ("디스크 N").
            for d in crate::io::list_disks() {
                drives.push(json!({
                    "name": disk_label(&d),
                    "path": d.path,
                    "dir": false,
                    "size": d.size,
                    "disk": true,
                    "readable": d.readable,
                }));
            }
            return Ok(json!({"path": "", "parent": null, "entries": drives}));
        }
        #[cfg(not(windows))]
        return ls("/");
    }
    let p = Path::new(path);
    let mut entries = vec![];
    for e in std::fs::read_dir(p).with_context(|| format!("cannot open {path}"))? {
        let Ok(e) = e else { continue };
        let Ok(m) = e.metadata() else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('$') {
            continue;
        }
        entries.push(json!({
            "name": name,
            "path": e.path().to_string_lossy(),
            "dir": m.is_dir(),
            "size": if m.is_file() { Some(m.len()) } else { None },
            "mtime": m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()),
        }));
    }
    entries.sort_by(|a, b| {
        (
            !a["dir"].as_bool().unwrap_or(false),
            a["name"].as_str().unwrap_or("").to_lowercase(),
        )
            .cmp(&(
                !b["dir"].as_bool().unwrap_or(false),
                b["name"].as_str().unwrap_or("").to_lowercase(),
            ))
    });
    let parent = p.parent().map(|x| x.to_string_lossy().into_owned());
    Ok(
        json!({"path": p.to_string_lossy(), "parent": parent.unwrap_or_default(), "entries": entries}),
    )
}

/// Free bytes on the volume holding `path` (or its nearest existing parent).
fn free_space(path: &str) -> Option<u64> {
    let mut p = Path::new(path);
    while !p.exists() {
        p = p.parent()?;
    }
    crate::io::free_space(p)
}

fn mkdir(parent: &str, name: &str, source: &str) -> Result<Value> {
    let name = name.trim();
    // Never write onto the disk being recovered.
    let src = Path::new(source);
    if let (true, Some(n), Some(c)) = (
        crate::io::is_device_path(src),
        crate::io::device_number(src),
        parent.chars().next().filter(|_| parent.get(1..2) == Some(":")),
    ) {
        anyhow::ensure!(
            crate::io::disk_of_letter(c.to_ascii_uppercase()) != Some(n),
            "이 위치는 복구할 디스크 {n} 안에 있어 폴더를 만들 수 없습니다"
        );
    }
    anyhow::ensure!(
        !name.is_empty()
            && name != "."
            && name != ".."
            && !name.chars().any(|c| "\\/:*?\"<>|".contains(c) || c.is_control()),
        "폴더 이름에 쓸 수 없는 글자가 있습니다: \\ / : * ? \" < > |"
    );
    let parent = Path::new(parent);
    anyhow::ensure!(parent.is_dir(), "{} 폴더가 없습니다", parent.display());
    let p = parent.join(name);
    std::fs::create_dir(&p).with_context(|| format!("cannot create {}", p.display()))?;
    Ok(json!({"path": p.to_string_lossy()}))
}

fn disk_label(d: &crate::io::Disk) -> String {
    let mut s = format!("💽 디스크 {}", d.number);
    if !d.model.is_empty() {
        s += &format!(" · {}", d.model);
    }
    if !d.letters.is_empty() {
        let l: Vec<String> = d.letters.iter().map(|c| format!("{c}:")).collect();
        s += &format!(" ({})", l.join(" "));
    }
    if d.removable {
        s += " · 이동식";
    }
    if !d.readable {
        s += " · 관리자 권한 필요";
    }
    s
}

fn info(path: &Path) -> Result<Value> {
    let src = Source::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut vols = vec![];
    let mut jdr = 0usize;
    for v in crate::volumes(&src)? {
        let fsi = crate::fs::analyze(&src, &v).ok().flatten();
        let j = fsi.as_ref().map_or(0, |f| {
            f.live
                .iter()
                .filter(|x| x.path.to_ascii_lowercase().ends_with(".jdr"))
                .count()
        });
        jdr += j;
        vols.push(json!({
            "offset": v.offset,
            "length": v.length,
            "fs": match v.kind {
                FsKind::Fat32 => "FAT32",
                FsKind::ExFat => "exFAT",
                FsKind::Ntfs => "NTFS",
                FsKind::Ext => "ext2/3/4",
                FsKind::Unknown => "알 수 없음",
            },
            "live_files": fsi.as_ref().map(|f| f.live_files),
            "deleted": fsi.as_ref().map(|f| f.deleted.len()),
            "jdr_files": j,
        }));
    }
    let device = crate::io::is_device_path(path);
    let disk = crate::io::device_number(path)
        .filter(|_| device)
        .and_then(|n| crate::io::list_disks().into_iter().find(|d| d.number == n));
    // For a disk there is no folder next to the image, so suggest
    // <folder of videorec>\..\videorec_out.
    let out_base = device
        .then(|| std::env::current_exe().ok())
        .flatten()
        .and_then(|e| e.parent().and_then(|p| p.parent()).map(|p| p.join("videorec_out")));
    Ok(json!({
        "path": path.to_string_lossy(),
        "name": match &disk {
            Some(d) => Some(disk_label(d)),
            None => path.file_name().map(|n| n.to_string_lossy().into_owned()),
        },
        "device": device,
        "disk_letters": disk.as_ref().map(|d| d.letters.iter().map(|c| c.to_string()).collect::<Vec<_>>()),
        "out_base": out_base.map(|p| p.to_string_lossy().into_owned()),
        "size": src.len(),
        "volumes": vols,
        "jdr_files": jdr,
        "recorder": crate::recorder::detect(&src)?,
    }))
}

fn start(b: &Value, job: &Shared) -> Result<Value> {
    let args: Vec<String> = b["args"]
        .as_array()
        .context("args missing")?
        .iter()
        .map(|a| a.as_str().unwrap_or("").to_string())
        .collect();
    anyhow::ensure!(
        matches!(args.first().map(|s| s.as_str()), Some("recover" | "export")),
        "only recover and export can be started"
    );
    let mut j = job.lock().unwrap();
    if let Some(c) = j.child.as_mut()
        && c.try_wait()?.is_none()
    {
        anyhow::bail!("a job is already running");
    }
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    *j = Job {
        child: Some(child),
        kind: args[0].clone(),
        args,
        started: Some(std::time::Instant::now()),
        ..Default::default()
    };
    drop(j);
    for pipe in [
        out.map(|p| Box::new(p) as Box<dyn Read + Send>),
        err.map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let job = job.clone();
        std::thread::spawn(move || pump(pipe, &job));
    }
    Ok(json!({"ok": true}))
}

/// Splits child output on `\r` and `\n`; progress lines start with `[`.
fn pump(mut p: Box<dyn Read + Send>, job: &Shared) {
    let mut buf = [0u8; 4096];
    let mut cur: Vec<u8> = vec![];
    loop {
        let n = match p.read(&mut buf) {
            Ok(0) | Err(_) => {
                let line = String::from_utf8_lossy(&cur).trim().to_string();
                if !line.is_empty() && !line.starts_with('[') {
                    job.lock().unwrap().lines.push(line);
                }
                break;
            }
            Ok(n) => n,
        };
        for &c in &buf[..n] {
            if c == b'\r' || c == b'\n' {
                let line = String::from_utf8_lossy(&cur).trim().to_string();
                cur.clear();
                if line.is_empty() {
                    continue;
                }
                let mut j = job.lock().unwrap();
                // A message printed right after a progress line (which has no
                // newline) ends up on the same line: split it off.
                let (line, extra) = match line.find("Error:") {
                    Some(k) if k > 0 && line.starts_with('[') => {
                        (line[..k].trim().to_string(), Some(line[k..].to_string()))
                    }
                    _ => (line, None),
                };
                if let Some(x) = extra {
                    j.lines.push(x);
                }
                if line.starts_with('[') {
                    j.progress = line;
                } else {
                    j.lines.push(line);
                    let n = j.lines.len();
                    if n > 300 {
                        j.lines.drain(..n - 300);
                    }
                }
            } else {
                cur.push(c);
            }
        }
    }
}

fn status(job: &Shared) -> Value {
    let mut j = job.lock().unwrap();
    let mut running = false;
    if let Some(c) = j.child.as_mut() {
        match c.try_wait() {
            Ok(None) => running = true,
            Ok(Some(st)) => {
                j.exit = Some(st.code().unwrap_or(-1));
                if j.took.is_none() {
                    j.took = j.started.map(|s| s.elapsed().as_secs());
                }
            }
            Err(_) => {}
        }
    }
    json!({
        "running": running,
        "kind": j.kind,
        "args": j.args,
        "exit": j.exit,
        "progress": j.progress,
        "elapsed_s": j.took.or(j.started.map(|s| s.elapsed().as_secs())),
        "lines": j.lines.iter().rev().take(60).rev().collect::<Vec<_>>(),
    })
}

fn open_path(p: &Path, select: bool) -> Result<Value> {
    anyhow::ensure!(p.exists(), "{} does not exist", p.display());
    #[cfg(windows)]
    {
        let mut c = Command::new("explorer.exe");
        if select {
            c.arg(format!("/select,{}", p.display()));
        } else {
            c.arg(p);
        }
        c.spawn()?;
    }
    #[cfg(not(windows))]
    {
        let target = if select { p.parent().unwrap_or(p) } else { p };
        Command::new("xdg-open").arg(target).spawn()?;
    }
    Ok(json!({"ok": true}))
}

/// Copies chosen output files (paths relative to `out`) into `dest`, keeping
/// their sub-folders, with a report of just those items.
fn copy(b: &Value) -> Result<Value> {
    let out = PathBuf::from(s_of(b, "out"));
    let dest = PathBuf::from(s_of(b, "dest"));
    anyhow::ensure!(!dest.as_os_str().is_empty(), "no destination");
    let report: Value = serde_json::from_str(&std::fs::read_to_string(out.join("report.json"))?)?;
    let want: std::collections::HashSet<String> = b["files"]
        .as_array()
        .context("files missing")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let items: Vec<Value> = report["items"]
        .as_array()
        .context("report has no items")?
        .iter()
        .filter(|it| want.contains(it["output"].as_str().unwrap_or("")))
        .cloned()
        .collect();
    std::fs::create_dir_all(&dest)?;
    let mut copied = 0u64;
    let mut bytes = 0u64;
    for it in &items {
        let rel = it["output"].as_str().unwrap_or("");
        anyhow::ensure!(
            !rel.contains("..") && !Path::new(rel).is_absolute(),
            "bad path {rel}"
        );
        let to = dest.join(rel);
        if let Some(d) = to.parent() {
            std::fs::create_dir_all(d)?;
        }
        bytes += std::fs::copy(out.join(rel), &to)?;
        copied += 1;
    }
    let sub = json!({"summary": {"copied_from": out.to_string_lossy(), "source": report["summary"]["source"], "items": copied}, "items": items});
    std::fs::write(
        dest.join("report.json"),
        serde_json::to_string_pretty(&sub)?,
    )?;
    Ok(json!({"copied": copied, "bytes": bytes, "dest": dest.to_string_lossy()}))
}
