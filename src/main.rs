#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use eframe::egui;
use egui::{Align2, Color32, RichText};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc,
    thread,
    time::Duration,
};
use uuid::Uuid;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, MoveWindow, ShowWindow, SW_HIDE, SW_SHOW, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_VISIBLE,
};

// ---------------------------- Data model ----------------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
enum SubType {
    M3u {
        url: String,
    },
    Xtream {
        host: String,
        username: String,
        password: String,
    },
    Mag {
        portal_url: String,
        mac: String,
        password: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct Subscription {
    id: String,
    name: String,
    #[serde(flatten)]
    kind: SubType,
    added: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Default)]
struct AppData {
    subs: Vec<Subscription>,
}

// ---------------------------- Background jobs ----------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum MpvStatus {
    Unknown,
    Checking,
    UpToDate,
    UpdateAvailable,
    NotInstalled,
}

#[derive(Debug)]
enum JobMsg {
    Status(String),
    Progress { current: u64, total: Option<u64> },
    Done(String),
    Error(String),
    MpvStatus(MpvStatus),
    MpvStarted(Child),
}

struct RunningJob {
    rx: mpsc::Receiver<JobMsg>,
}

// ---------------------------- App state ----------------------------

struct MpvPlayer {
    child: Option<Child>,
    child_hwnd: Option<isize>,
    embedded: bool,
    ipc_path: String,
}

impl MpvPlayer {
    fn new() -> Self {
        Self {
            child: None,
            child_hwnd: None,
            embedded: true,
            ipc_path: r"\\.\pipe\mpv-ipc".to_string(),
        }
    }

    fn send_command(&self, cmd: &str) -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.ipc_path)
            .context("Nu pot deschide pipe-ul MPV")?;
        file.write_all(cmd.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn ensure_child_window(&mut self, parent_hwnd: isize) {
        if self.child_hwnd.is_none() {
            unsafe {
                let class_name: Vec<u16> = "Static\0".encode_utf16().collect();
                let hwnd = CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    std::ptr::null(),
                    WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
                    0,
                    0,
                    0,
                    0,
                    parent_hwnd as _,
                    0,
                    0,
                    std::ptr::null(),
                );
                if hwnd != 0 {
                    self.child_hwnd = Some(hwnd as _);
                }
            }
        }
    }

    fn set_visible(&self, visible: bool) {
        if let Some(hwnd) = self.child_hwnd {
            unsafe {
                ShowWindow(hwnd as _, if visible { SW_SHOW } else { SW_HIDE });
            }
        }
    }

    fn move_window(&self, rect: egui::Rect, pixels_per_point: f32) {
        if let Some(hwnd) = self.child_hwnd {
            unsafe {
                MoveWindow(
                    hwnd as _,
                    (rect.min.x * pixels_per_point) as i32,
                    (rect.min.y * pixels_per_point) as i32,
                    (rect.width() * pixels_per_point) as i32,
                    (rect.height() * pixels_per_point) as i32,
                    1,
                );
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(None) => true,
                _ => {
                    self.child = None;
                    false
                }
            }
        } else {
            false
        }
    }
}

struct App {
    data: AppData,
    data_path: PathBuf,
    playlists_dir: PathBuf,
    mpv_dir: PathBuf,
    mpv_status: MpvStatus,
    player: MpvPlayer,

    selected_id: Option<String>,

    status: String,
    progress_current: u64,
    progress_total: Option<u64>,
    job: Option<RunningJob>,

    // dialogs
    show_m3u: bool,
    show_xt: bool,
    show_mag: bool,

    // form fields
    form_name: String,
    form_url: String,
    form_host: String,
    form_user: String,
    form_pass: String,
    form_portal: String,
    form_mac: String,
    form_mag_pass: String,
}

#[allow(dead_code)]
impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let base = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("iptv-manager");

        let playlists_dir = base.join("playlists");
        let mpv_dir = base.join("mpv");
        let data_path = base.join("subscriptions.json");

        let _ = fs::create_dir_all(&base);
        let _ = fs::create_dir_all(&playlists_dir);
        let _ = fs::create_dir_all(&mpv_dir);

        let data = fs::read_to_string(&data_path)
            .ok()
            .and_then(|s| serde_json::from_str::<AppData>(&s).ok())
            .unwrap_or_default();

        let mut app = Self {
            data,
            data_path,
            playlists_dir,
            mpv_dir,
            mpv_status: MpvStatus::Unknown,
            player: MpvPlayer::new(),

            selected_id: None,

            status: "Gata".to_string(),
            progress_current: 0,
            progress_total: None,
            job: None,

            show_m3u: false,
            show_xt: false,
            show_mag: false,

            form_name: String::new(),
            form_url: String::new(),
            form_host: String::new(),
            form_user: String::new(),
            form_pass: String::new(),
            form_portal: String::new(),
            form_mac: String::new(),
            form_mag_pass: String::new(),
        };
        app.start_check_mpv_status();
        app
    }

    // ---------------------------- helpers ----------------------------

    fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    fn reset_progress(&mut self) {
        self.progress_current = 0;
        self.progress_total = None;
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.data)?;
        fs::write(&self.data_path, json)?;
        Ok(())
    }

    fn selected_sub(&self) -> Option<&Subscription> {
        let id = self.selected_id.as_deref()?;
        self.data.subs.iter().find(|s| s.id == id)
    }

    fn clear_form(&mut self) {
        self.form_name.clear();
        self.form_url.clear();
        self.form_host.clear();
        self.form_user.clear();
        self.form_pass.clear();
        self.form_portal.clear();
        self.form_mac.clear();
        self.form_mag_pass.clear();
    }

    fn sanitized_filename(name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn playlist_path_for(sub: &Subscription, playlists_dir: &Path) -> PathBuf {
        let safe = Self::sanitized_filename(&sub.name);
        playlists_dir.join(format!("{safe}.m3u"))
    }

    fn mpv_exe_path(mpv_dir: &Path) -> PathBuf {
        mpv_dir.join("mpv.exe")
    }

    fn mpv_version_file(mpv_dir: &Path) -> PathBuf {
        mpv_dir.join("version.txt")
    }

    fn read_installed_mpv_version(mpv_dir: &Path) -> Option<String> {
        fs::read_to_string(Self::mpv_version_file(mpv_dir))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn find_file_recursive(root: &Path, filename: &str, max_depth: usize) -> Option<PathBuf> {
        fn inner(dir: &Path, filename: &str, depth: usize) -> Option<PathBuf> {
            if depth == 0 {
                return None;
            }
            let rd = fs::read_dir(dir).ok()?;
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_file() {
                    if let Some(n) = p.file_name().and_then(|x| x.to_str()) {
                        if n.eq_ignore_ascii_case(filename) {
                            return Some(p);
                        }
                    }
                } else if p.is_dir() {
                    if let Some(found) = inner(&p, filename, depth - 1) {
                        return Some(found);
                    }
                }
            }
            None
        }
        inner(root, filename, max_depth)
    }

    fn build_playlist_url(sub: &Subscription) -> String {
        match &sub.kind {
            SubType::M3u { url } => url.clone(),
            SubType::Xtream {
                host,
                username,
                password,
            } => format!(
                "{}/get.php?username={}&password={}&type=m3u_plus",
                host.trim_end_matches('/'),
                username,
                password
            ),
            SubType::Mag {
                portal_url,
                mac,
                password,
            } => format!(
                "{}/panel_api.php?mac={}&password={}",
                portal_url.trim_end_matches('/'),
                mac,
                password.clone().unwrap_or_default()
            ),
        }
    }

    fn http_client() -> Result<Client> {
        Client::builder()
            .redirect(reqwest::redirect::Policy::limited(10))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
            .build()
            .context("Nu pot inițializa clientul HTTP")
    }

    fn http_get_with_common_headers<'a>(
        client: &'a Client,
        url: &'a str,
    ) -> reqwest::blocking::RequestBuilder {
        client
            .get(url)
            .header("Accept", "*/*")
            .header("Connection", "keep-alive")
            .header("Referer", "https://sourceforge.net/")
    }

    fn download_to_file_with_progress(
        client: &Client,
        url: &str,
        out_path: &Path,
        tx: &mpsc::Sender<JobMsg>,
    ) -> Result<()> {
        let mut resp = Self::http_get_with_common_headers(client, url)
            .send()
            .with_context(|| format!("Descărcarea a eșuat: {url}"))?;

        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} la {}", resp.status(), url));
        }

        let total = resp.content_length();

        let mut file = fs::File::create(out_path)
            .with_context(|| format!("Nu pot crea fișier: {}", out_path.display()))?;

        // copy the entire response into the file (returns u64 bytes copied)
        let copied = std::io::copy(&mut resp, &mut file).context("Eroare la copiere stream")?;
        let _ = tx.send(JobMsg::Progress {
            current: copied,
            total,
        });

        Ok(())
    }

    // Versiune (best-effort) fără a descărca tot fișierul.
    // Dacă nu se poate, returnează None și folosim "latest".
    fn probe_latest_mpv_version(client: &Client, url: &str) -> Option<String> {
        if let Ok(resp) = client.head(url).send() {
            if resp.status().is_success() {
                return Some(Self::version_from_url(resp.url().as_str()));
            }
        }

        if let Ok(resp) = Self::http_get_with_common_headers(client, url)
            .header("Range", "bytes=0-0")
            .send()
        {
            if resp.status().is_success() || resp.status().as_u16() == 206 {
                return Some(Self::version_from_url(resp.url().as_str()));
            }
        }

        None
    }

    fn version_from_url(final_url: &str) -> String {
        let no_q = final_url.split('?').next().unwrap_or(final_url);
        no_q.rsplit('/')
            .next()
            .unwrap_or("latest")
            .trim()
            .to_string()
    }

    fn start_job(&mut self, f: impl FnOnce(mpsc::Sender<JobMsg>) + Send + 'static) {
        if self.is_busy() {
            self.status = "Există deja o acțiune în desfășurare.".to_string();
            return;
        }
        self.reset_progress();
        let (tx, rx) = mpsc::channel::<JobMsg>();
        self.job = Some(RunningJob { rx });
        thread::spawn(move || f(tx));
    }

    fn start_check_mpv_status(&mut self) {
        if self.is_busy() {
            return;
        }

        self.mpv_status = MpvStatus::Checking;
        let mpv_dir = self.mpv_dir.clone();

        self.start_job(move |tx| {
            let res = (|| -> Result<()> {
                let client = Self::http_client()?;
                let (latest_version, _) = Self::fetch_latest_mpv_info(&client)?;

                let mpv_exe = Self::mpv_exe_path(&mpv_dir);
                if !mpv_exe.exists() {
                    let _ = tx.send(JobMsg::MpvStatus(MpvStatus::NotInstalled));
                    return Ok(());
                }

                let installed_version =
                    Self::read_installed_mpv_version(&mpv_dir).unwrap_or_default();
                if installed_version == latest_version {
                    let _ = tx.send(JobMsg::MpvStatus(MpvStatus::UpToDate));
                } else {
                    let _ = tx.send(JobMsg::MpvStatus(MpvStatus::UpdateAvailable));
                }
                Ok(())
            })();

            if let Err(_) = res {
                let exe = Self::mpv_exe_path(&mpv_dir);
                if !exe.exists() {
                    let _ = tx.send(JobMsg::MpvStatus(MpvStatus::NotInstalled));
                } else {
                    let _ = tx.send(JobMsg::MpvStatus(MpvStatus::Unknown));
                }
            }
            let _ = tx.send(JobMsg::Done("Verificare MPV finalizată.".to_string()));
        });
    }

    fn download_and_extract_mpv(
        client: &Client,
        download_url: &str,
        mpv_dir: &Path,
        latest_version: &str,
        tx: &mpsc::Sender<JobMsg>,
    ) -> Result<PathBuf> {
        tx.send(JobMsg::Status(format!("Descarc mpv: {}...", latest_version)))
            .ok();
        fs::create_dir_all(mpv_dir)?;
        let archive_path = mpv_dir.join("mpv.7z");

        let mut resp = client.get(download_url).send()?;
        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} la mpv", resp.status()));
        }

        let mut file = fs::File::create(&archive_path)?;
        let total = resp.content_length();
        let mut downloaded: u64 = 0;
        let mut buf = [0u8; 64 * 1024];

        while let Ok(n) = resp.read(&mut buf) {
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            downloaded += n as u64;
            tx.send(JobMsg::Progress {
                current: downloaded,
                total,
            })
            .ok();
        }

        tx.send(JobMsg::Status("Extrag mpv...".into())).ok();
        sevenz_rust::decompress_file(&archive_path, mpv_dir)
            .map_err(|e| anyhow!("Eroare extragere: {e}"))?;

        fs::remove_file(&archive_path).ok();
        fs::write(Self::mpv_version_file(mpv_dir), latest_version).ok();
        tx.send(JobMsg::MpvStatus(MpvStatus::UpToDate)).ok();

        Self::find_file_recursive(mpv_dir, "mpv.exe", 6)
            .ok_or_else(|| anyhow!("Nu găsesc mpv.exe după extragere"))
    }

    fn start_mpv_maintenance_job(&mut self) {
        if self.is_busy() {
            return;
        }

        let mpv_dir = self.mpv_dir.clone();
        let current_status = self.mpv_status;

        self.start_job(move |tx| {
            if let Err(e) = (|| -> Result<()> {
                let client = Self::http_client()?;

                tx.send(JobMsg::Status("Caut ultima versiune mpv...".into()))
                    .ok();
                let (latest_version, download_url) = Self::fetch_latest_mpv_info(&client)?;

                let mpv_exe = Self::mpv_exe_path(&mpv_dir);
                let installed_version =
                    Self::read_installed_mpv_version(&mpv_dir).unwrap_or_default();

                if mpv_exe.exists() && installed_version == latest_version {
                    tx.send(JobMsg::MpvStatus(MpvStatus::UpToDate)).ok();
                    tx.send(JobMsg::Done("MPV este deja la zi.".into())).ok();
                    return Ok(());
                }

                let exe =
                    Self::download_and_extract_mpv(&client, &download_url, &mpv_dir, &latest_version, &tx)?;

                if current_status == MpvStatus::NotInstalled {
                    tx.send(JobMsg::Status("Pornesc mpv...".into())).ok();
                    let _ = Command::new(exe).spawn();
                }

                tx.send(JobMsg::Done("Actualizare MPV finalizată.".into()))
                    .ok();
                Ok(())
            })() {
                tx.send(JobMsg::Error(e.to_string())).ok();
            }
        });
    }

    fn poll_job_messages(&mut self) {
        // take ownership -> no borrow conflicts
        let job = match self.job.take() {
            Some(j) => j,
            _ => return,
        };

        let mut done: Option<String> = None;
        let mut err: Option<String> = None;
        let mut disconnected = false;

        loop {
            match job.rx.try_recv() {
                Ok(msg) => match msg {
                    JobMsg::Status(s) => self.status = s,
                    JobMsg::Progress { current, total } => {
                        self.progress_current = current;
                        self.progress_total = total;
                    }
                    JobMsg::Done(s) => done = Some(s),
                    JobMsg::Error(e) => err = Some(e),
                    JobMsg::MpvStatus(s) => self.mpv_status = s,
                    JobMsg::MpvStarted(c) => self.player.child = Some(c),
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        if let Some(e) = err {
            self.status = format!("Eroare: {e}");
            self.reset_progress();
            self.job = None;
            return;
        }

        if let Some(s) = done {
            self.status = s;
            self.reset_progress();
            self.job = None;
            return;
        }

        if disconnected {
            self.reset_progress();
            self.job = None;
            return;
        }

        self.job = Some(job);
    }

    fn fetch_latest_mpv_info(client: &Client) -> Result<(String, String)> {
        let api_url = "https://api.github.com/repos/shinchiro/mpv-winbuild-cmake/releases/latest";

        let resp = client
            .get(api_url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "iptv-manager")
            .send()
            .context("Nu pot interoga GitHub API")?;

        if !resp.status().is_success() {
            return Err(anyhow!("HTTP {} la GitHub API", resp.status()));
        }

        let json: serde_json::Value = resp.json().context("JSON invalid GitHub")?;

        let assets = json["assets"]
            .as_array()
            .ok_or_else(|| anyhow!("Nu există assets în release"))?;

        for asset in assets {
            let name = asset["name"].as_str().unwrap_or("");
            let url = asset["browser_download_url"].as_str().unwrap_or("");

            if name.contains("mpv-x86_64") && name.ends_with(".7z") && !name.contains("ffmpeg") {
                return Ok((name.to_string(), url.to_string()));
            }
        }

        Err(anyhow!("Nu am găsit asset mpv x86_64 .7z"))
    }


    // ---------------------------- actions ----------------------------

    fn start_download_playlist(&mut self) {
        let sub = match self.selected_sub() {
            Some(s) => s.clone(),
            _ => {
                self.status = "Selectează un abonament.".to_string();
                return;
            }
        };

        let playlists_dir = self.playlists_dir.clone();
        self.status = "Descarc playlist...".to_string();

        self.start_job(move |tx| {
            if let Err(e) = (|| -> Result<()> {
                let url = Self::build_playlist_url(&sub);
                let client = Self::http_client()?;

                let _ = tx.send(JobMsg::Status("Conectare...".to_string()));

                let mut resp = client
                    .get(&url)
                    .header("Accept", "*/*")
                    .header("Connection", "keep-alive")
                    .send()
                    .context("Cererea HTTP a eșuat")?;

                if !resp.status().is_success() {
                    return Err(anyhow!("HTTP {}", resp.status()));
                }

                let total = resp.content_length();

                let path = Self::playlist_path_for(&sub, &playlists_dir);
                let mut file = fs::File::create(&path).context("Nu pot crea fișierul playlist")?;

                // copy the entire response into the file (returns u64 bytes copied)
                let copied = std::io::copy(&mut resp, &mut file).context("Eroare la copiere")?;
                let _ = tx.send(JobMsg::Progress {
                    current: copied,
                    total,
                });

                let _ = tx.send(JobMsg::Done(format!("Playlist salvat: {}", path.display())));
                Ok(())
            })() {
                let _ = tx.send(JobMsg::Error(e.to_string()));
            }
        });
    }
    
    fn start_ensure_mpv_and_play(&mut self) {
        let sub = match self.selected_sub() {
            Some(s) => s.clone(),
            _ => {
                self.status = "Selectează un abonament.".to_string();
                return;
            }
        };

        let playlists_dir = self.playlists_dir.clone();
        let mpv_dir = self.mpv_dir.clone();

        let playlist_path = Self::playlist_path_for(&sub, &playlists_dir);
        if !playlist_path.exists() {
            self.status = "Playlist-ul nu există. Apasă „Descarcă” întâi.".to_string();
            return;
        }

        // Check if alive first (need to do it before moving player to thread)
        let mut is_alive = self.player.is_alive();
        let ipc_path = self.player.ipc_path.clone();
        let embedded = self.player.embedded;
        let wid = self.player.child_hwnd;

        self.start_job(move |tx| {
            if let Err(e) = (|| -> Result<()> {
                let client = Self::http_client()?;

                tx.send(JobMsg::Status("Verific mpv...".into())).ok();
                let (latest_version, download_url) = Self::fetch_latest_mpv_info(&client)?;

                let mpv_exe_local = Self::mpv_exe_path(&mpv_dir);
                let installed_version =
                    Self::read_installed_mpv_version(&mpv_dir).unwrap_or_default();

                let mpv_exe = if mpv_exe_local.exists() && installed_version == latest_version {
                    mpv_exe_local
                } else {
                    let exe = Self::download_and_extract_mpv(&client, &download_url, &mpv_dir, &latest_version, &tx)?;
                    is_alive = false; // forced new spawn if updated
                    exe
                };

                if is_alive {
                    tx.send(JobMsg::Status("Încarc playlist în MPV existent...".into())).ok();
                    let file_arg = playlist_path.to_string_lossy().replace("\\", "\\\\");
                    let cmd = format!("{{\"command\": [\"loadfile\", \"{}\"]}}", file_arg);
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&ipc_path)
                        .context("Nu pot deschide pipe-ul MPV")?;
                    file.write_all(cmd.as_bytes())?;
                    file.write_all(b"\n")?;
                } else {
                    tx.send(JobMsg::Status("Pornesc MPV...".into())).ok();
                    let mut cmd = Command::new(mpv_exe);
                    cmd.arg(playlist_path)
                       .arg("--force-window=yes")
                       .arg(format!("--input-ipc-server={}", ipc_path));

                    if embedded {
                        if let Some(w) = wid {
                            cmd.arg(format!("--wid={}", w));
                        }
                    }

                    let child = cmd.spawn()?;
                    let _ = tx.send(JobMsg::MpvStarted(child));
                }

                tx.send(JobMsg::Done("Redare pornită.".into())).ok();
                Ok(())
            })() {
                tx.send(JobMsg::Error(e.to_string())).ok();
            }
        });
    }

    fn delete_selected(&mut self) {
        let Some(id) = self.selected_id.clone() else {
            self.status = "Selectează un abonament.".to_string();
            return;
        };

        let before = self.data.subs.len();
        self.data.subs.retain(|s| s.id != id);
        let after = self.data.subs.len();

        self.selected_id = None;

        if before == after {
            self.status = "Nu am găsit abonamentul pentru ștergere.".to_string();
            return;
        }

        if let Err(e) = self.save() {
            self.status = format!("Eroare la salvare: {e}");
            return;
        }

        self.status = "Abonament șters.".to_string();
    }

    fn add_subscription(&mut self, kind: SubType) {
        let name = self.form_name.trim().to_string();
        if name.is_empty() {
            self.status = "Numele este obligatoriu.".to_string();
            return;
        }

        self.data.subs.push(Subscription {
            id: Uuid::new_v4().to_string(),
            name: name.clone(),
            kind,
            added: Utc::now(),
        });

        if let Err(e) = self.save() {
            self.status = format!("Eroare la salvare: {e}");
            return;
        }

        self.status = format!("Adăugat: {name}");
        self.clear_form();
    }
}

// ---------------------------- UI ----------------------------

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_job_messages();

        if let Ok(handle) = _frame.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                let hwnd = h.hwnd.get();
                self.player.ensure_child_window(hwnd);
            }
        }

        egui::TopBottomPanel::top("top_menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Adaugă", |ui| {
                    if ui.button("M3U").clicked() {
                        self.show_m3u = true;
                        ui.close_menu();
                    }
                    if ui.button("Xtream Code").clicked() {
                        self.show_xt = true;
                        ui.close_menu();
                    }
                    if ui.button("MAG").clicked() {
                        self.show_mag = true;
                        ui.close_menu();
                    }
                });

                ui.separator();

                let (btn_text, use_custom_style) = match self.mpv_status {
                    MpvStatus::NotInstalled => ("Instalează MPV", true),
                    MpvStatus::UpdateAvailable => ("Actualizează MPV", true),
                    MpvStatus::Checking => ("Verificare MPV...", false),
                    MpvStatus::UpToDate => ("MPV este la zi", false),
                    MpvStatus::Unknown => ("Verifică MPV", false),
                };

                let mut btn = egui::Button::new(if use_custom_style {
                    RichText::new(btn_text).color(Color32::BLACK)
                } else {
                    RichText::new(btn_text)
                });

                if use_custom_style {
                    btn = btn.fill(Color32::GREEN);
                }

                if ui.add_enabled(!self.is_busy(), btn).clicked() {
                    self.start_mpv_maintenance_job();
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("IPTV Manager");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Abonamente:");
                ui.add_space(8.0);

                if ui.button("Șterge selectat").clicked() && !self.is_busy() {
                    self.delete_selected();
                }
            });

            egui::ScrollArea::vertical()
                .max_height(240.0)
                .show(ui, |ui| {
                    for s in &self.data.subs {
                        let t = match s.kind {
                            SubType::M3u { .. } => "M3U",
                            SubType::Xtream { .. } => "Xtream",
                            SubType::Mag { .. } => "MAG",
                        };

                        let label = format!("{} ({}) — {}", s.name, t, s.added.format("%Y-%m-%d"));
                        let selected = self.selected_id.as_deref() == Some(&s.id);

                        if ui.selectable_label(selected, label).clicked() {
                            self.selected_id = Some(s.id.clone());
                        }
                    }
                });

            ui.separator();

            let can_click = !self.is_busy() && self.selected_id.is_some();

            ui.horizontal(|ui| {
                ui.add_enabled_ui(can_click, |ui| {
                    if ui.button("Descarcă playlist").clicked() {
                        self.start_download_playlist();
                    }
                    if ui.button("Redă cu mpv").clicked() {
                        self.start_ensure_mpv_and_play();
                    }
                });

                ui.separator();

                if ui
                    .button(if self.player.embedded {
                        "Mod Extern"
                    } else {
                        "Mod Embed"
                    })
                    .clicked()
                {
                    self.player.embedded = !self.player.embedded;
                    if let Some(hwnd) = self.player.child_hwnd {
                        let wid = if self.player.embedded { hwnd } else { 0 };
                        let _ = self.player.send_command(&format!(
                            "{{\"command\": [\"set_property\", \"wid\", {}]}}",
                            wid
                        ));
                    }
                }

                if ui.button("Fullscreen").clicked() {
                    let _ = self.player.send_command("{\"command\": [\"cycle\", \"fullscreen\"]}");
                }

                if self.is_busy() {
                    ui.spinner();
                }
            });

            if self.is_busy() {
                let frac = match self.progress_total {
                    Some(t) if t > 0 => (self.progress_current as f32 / t as f32).clamp(0.0, 1.0),
                    _ => 0.0,
                };

                if let Some(t) = self.progress_total {
                    ui.add(egui::ProgressBar::new(frac).text(format!(
                        "{} / {} MB",
                        self.progress_current / 1_000_000,
                        t / 1_000_000
                    )));
                } else if self.progress_current > 0 {
                    ui.add(
                        egui::ProgressBar::new(frac)
                            .text(format!("{} MB", self.progress_current / 1_000_000)),
                    );
                }
            }

            ui.separator();

            let player_active = self.player.is_alive();

            if self.player.embedded && player_active {
                let available = ui.available_size();
                let (rect, _response) = ui.allocate_at_least(
                    egui::vec2(available.x, available.y.max(300.0)),
                    egui::Sense::hover(),
                );

                self.player.move_window(rect, ctx.pixels_per_point());
                self.player.set_visible(true);
            } else {
                self.player.set_visible(false);
            }

            ui.separator();

            let is_err = self.status.to_lowercase().contains("eroare");
            ui.colored_label(
                if is_err { Color32::RED } else { Color32::GREEN },
                &self.status,
            );
        });

        // Dialog M3U (fără borrow conflict pe open)
        if self.show_m3u {
            let mut should_close = false;
            let mut should_add = false;

            egui::Window::new("Adaugă M3U")
                .open(&mut self.show_m3u)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.form_name).hint_text("Nume"));
                    ui.add(egui::TextEdit::singleline(&mut self.form_url).hint_text("URL M3U"));

                    ui.horizontal(|ui| {
                        if ui.button("Anulează").clicked() {
                            should_close = true;
                        }
                        if ui.button("Adaugă").clicked() {
                            should_add = true;
                            should_close = true;
                        }
                    });
                });

            if should_close {
                self.show_m3u = false;
            }

            if should_add {
                if self.form_url.trim().is_empty() {
                    self.status = "URL M3U este obligatoriu.".to_string();
                } else {
                    self.add_subscription(SubType::M3u {
                        url: self.form_url.trim().to_string(),
                    });
                }
            }
        }

        // Dialog Xtream
        if self.show_xt {
            let mut should_close = false;
            let mut should_add = false;

            egui::Window::new("Adaugă Xtream Code")
                .open(&mut self.show_xt)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.form_name).hint_text("Nume"));
                    ui.add(egui::TextEdit::singleline(&mut self.form_host).hint_text("Host"));
                    ui.add(egui::TextEdit::singleline(&mut self.form_user).hint_text("Username"));
                    ui.add(egui::TextEdit::singleline(&mut self.form_pass).hint_text("Password"));

                    ui.horizontal(|ui| {
                        if ui.button("Anulează").clicked() {
                            should_close = true;
                        }
                        if ui.button("Adaugă").clicked() {
                            should_add = true;
                            should_close = true;
                        }
                    });
                });

            if should_close {
                self.show_xt = false;
            }

            if should_add {
                if self.form_host.trim().is_empty()
                    || self.form_user.trim().is_empty()
                    || self.form_pass.trim().is_empty()
                {
                    self.status = "Completează toate câmpurile.".to_string();
                } else {
                    self.add_subscription(SubType::Xtream {
                        host: self.form_host.trim().to_string(),
                        username: self.form_user.trim().to_string(),
                        password: self.form_pass.trim().to_string(),
                    });
                }
            }
        }

        // Dialog MAG
        if self.show_mag {
            let mut should_close = false;
            let mut should_add = false;

            egui::Window::new("Adaugă MAG")
                .open(&mut self.show_mag)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.form_name).hint_text("Nume"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form_portal).hint_text("Portal URL"),
                    );
                    ui.add(egui::TextEdit::singleline(&mut self.form_mac).hint_text("MAC"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.form_mag_pass)
                            .hint_text("Parolă (opțional)"),
                    );

                    ui.horizontal(|ui| {
                        if ui.button("Anulează").clicked() {
                            should_close = true;
                        }
                        if ui.button("Adaugă").clicked() {
                            should_add = true;
                            should_close = true;
                        }
                    });
                });

            if should_close {
                self.show_mag = false;
            }

            if should_add {
                if self.form_portal.trim().is_empty() || self.form_mac.trim().is_empty() {
                    self.status = "Portal URL și MAC sunt obligatorii.".to_string();
                } else {
                    let pass = self.form_mag_pass.trim();
                    let pass_opt = if pass.is_empty() {
                        None
                    } else {
                        Some(pass.to_string())
                    };

                    self.add_subscription(SubType::Mag {
                        portal_url: self.form_portal.trim().to_string(),
                        mac: self.form_mac.trim().to_string(),
                        password: pass_opt,
                    });
                }
            }
        }

        if self.is_busy() || self.player.is_alive() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(mut child) = self.player.child.take() {
            let _ = child.kill();
        }
        if let Some(hwnd) = self.player.child_hwnd {
            unsafe {
                DestroyWindow(hwnd as _);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 680.0])
            .with_title("IPTV Manager"),
        ..Default::default()
    };

    eframe::run_native(
        "IPTV Manager",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
