//! RClauncher
//!   * デスクトップの何もない場所でダブルクリック : フォルダ一覧 → Explorerで開く
//!   * アプリ画面内で右クリック2回               : クリップボード履歴 → 貼り付け
//!
//! 注意: windows クレート 0.58.0 向け。バージョンを上げるとシグネチャが変わります。

#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering::Relaxed};

use serde::Deserialize;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, ScreenToClient,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HBRUSH, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_FLAGS_AND_ATTRIBUTES};
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Threading::{
    AttachThreadInput, CreateMutexW, GetCurrentThreadId, OpenProcess, Sleep, PROCESS_VM_OPERATION,
    PROCESS_VM_WRITE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod settings; // 設定ダイアログ・自動起動
mod shellmenu; // Explorer の右クリックメニュー

// ───────────────────────── 定数 ─────────────────────────

const CF_UNICODETEXT: u32 = 13;
const WM_APP_TRIGGER: u32 = WM_APP + 1; // フック → メインへの通知
const WM_APP_TRAY: u32 = WM_APP + 2; // タスクトレイのコールバック
const WM_APP_TRIGGER_FOLDER: u32 = WM_APP + 3; // デスクトップのダブルクリック
const WM_APP_MENU_CLICK: u32 = WM_APP + 4; // フォルダメニュー表示中の左クリック通知
const TIMER_SHOW: usize = 1;
const TIMER_CFG: usize = 2; // 設定ファイルの更新監視 (2秒ごと)
const LVM_HITTEST: u32 = 0x1012; // LVM_FIRST + 18

/// LVHITTESTINFO (Explorerのプロセス内に書き込んで LVM_HITTEST に渡す)
#[repr(C)]
struct LvHitTestInfo {
    pt: POINT,
    flags: u32,
    item: i32,
    sub_item: i32,
    group: i32,
}

const CLIP_BASE: u32 = 1000; // クリップボード項目のメニューID (最大 500 件)
const FOLDER_BASE: u32 = 5000; // フォルダ項目のメニューID
const CMD_RELOAD: u32 = 9001;
const CMD_EXIT: u32 = 9002;
const CMD_SETTINGS: u32 = 9003;
const CMD_OPEN_CFG: u32 = 9004;
const CMD_OPEN_CFG_DIR: u32 = 9005;
const CMD_AUTOSTART: u32 = 9006;

const MAX_ENTRY_CHARS: usize = 100_000; // これを超えるテキストは履歴に入れない
const PREVIEW_CHARS: usize = 10; // メニュー表示は1行・10文字まで (超えたら末尾に ...)
const CMD_CLIP_CLEAR: u32 = 4000; // クリップボードの「履歴クリア」のメニューID
const CMD_FHIST_CLEAR: u32 = 4001; // フォルダ履歴の「履歴クリア」のメニューID

const DEFAULT_CONFIG: &str = include_str!("../config.toml");

// ───────────────────────── 設定 ─────────────────────────

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct Config {
    general: General,
    clipboard: ClipCfg,
    folder_menu: FolderMenuCfg,
    folders: Vec<Folder>,
}

#[derive(Deserialize, Clone)]
#[serde(default)]
struct FolderMenuCfg {
    max_entries: usize, // 1つのサブメニューに出す最大フォルダ数
    show_hidden: bool,  // 隠し/システムフォルダを表示するか
    history_max: usize, // フォルダ履歴の件数 (0 = 履歴を使わない)
    show_icons: bool,   // メニューにフォルダアイコンを表示するか
}
impl Default for FolderMenuCfg {
    fn default() -> Self {
        Self { max_entries: 300, show_hidden: false, history_max: 30, show_icons: true }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
struct General {
    double_click_ms: u32,
}
impl Default for General {
    fn default() -> Self {
        Self { double_click_ms: 0 }
    }
}

#[derive(Deserialize, Clone)]
#[serde(default)]
struct ClipCfg {
    max_items: usize,
    group_size: usize,
}
impl Default for ClipCfg {
    fn default() -> Self {
        Self { max_items: 200, group_size: 20 }
    }
}

#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct Folder {
    name: String,
    path: String,
}

fn config_path() -> PathBuf {
    std::env::current_exe()
        .map(|p| p.with_file_name("config.toml"))
        .unwrap_or_else(|_| PathBuf::from("config.toml"))
}

fn load_config() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            // 初回起動: サンプル設定を書き出す
            let _ = std::fs::write(&path, DEFAULT_CONFIG);
            DEFAULT_CONFIG.to_string()
        }
    };
    let mut cfg: Config = toml::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("config.toml の読み込みに失敗しました:\n{e}"))?;
    cfg.clipboard.max_items = cfg.clipboard.max_items.clamp(1, 500);
    cfg.clipboard.group_size = cfg.clipboard.group_size.clamp(1, 50);
    cfg.folder_menu.max_entries = cfg.folder_menu.max_entries.clamp(10, 1000);
    cfg.folder_menu.history_max = cfg.folder_menu.history_max.min(100);
    Ok(cfg)
}

// ───────────────────────── 履歴ファイル (clipboard/history.txt) ─────────────────────────
// 形式: UTF-8、1行 = 1エントリ、古い順 (末尾が最新)。
//       行内の \ は \\、改行は \n、復帰は \r にエスケープして保存する。

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn history_path() -> PathBuf {
    exe_dir().join("clipboard").join("history.txt")
}

/// 起動時: clipboard フォルダ(と空の history.txt)が無ければ作成する
fn ensure_clipboard_dir() {
    let _ = std::fs::create_dir_all(exe_dir().join("clipboard"));
    let path = history_path();
    if !path.exists() {
        let _ = std::fs::write(&path, b"");
    }
}

fn escape_line(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            _ => o.push(c),
        }
    }
    o
}

fn unescape_line(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some('\\') => o.push('\\'),
            Some(other) => {
                o.push('\\');
                o.push(other);
            }
            None => o.push('\\'),
        }
    }
    o
}

/// history.txt を読む。戻り値は古い順。重複は新しい方を残す。
fn read_history_file() -> Option<Vec<String>> {
    let bytes = std::fs::read(history_path()).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let text: &str = text.trim_start_matches('\u{feff}');
    let mut all: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let t = unescape_line(line);
        if !t.trim().is_empty() {
            all.push(t);
        }
    }
    let mut seen = HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(all.len());
    for t in all.into_iter().rev() {
        if seen.insert(t.clone()) {
            out.push(t);
        }
    }
    out.reverse();
    Some(out)
}

fn append_history_file(text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(history_path()) {
        let _ = f.write_all(format!("{}\n", escape_line(text)).as_bytes());
    }
}

/// 履歴全体を書き直す (history は新しい順)。一時ファイル経由で置き換える。
fn write_history_file(history: &VecDeque<String>) {
    let mut buf = String::new();
    for t in history.iter().rev() {
        buf.push_str(&escape_line(t));
        buf.push('\n');
    }
    let path = history_path();
    let tmp = path.with_extension("txt.tmp");
    if std::fs::write(&tmp, buf.as_bytes()).is_ok() {
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::write(&path, buf.as_bytes());
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// 履歴クリア: history.txt を空にし、メモリ上の履歴も消す
fn clear_history() {
    let _ = std::fs::write(history_path(), b"");
    with_state(|s| s.history.clear());
}

/// history.txt の内容をメモリへ読み込む (新しい順)
fn sync_history_from_file() {
    if let Some(v) = read_history_file() {
        with_state(|s| {
            let m = s.cfg.clipboard.max_items;
            s.history = v.into_iter().rev().take(m).collect();
        });
    }
}

// ───────────────────────── 状態 ─────────────────────────
// メニュー表示中(モーダルループ中)も WM_CLIPBOARDUPDATE 等が届くため、
// borrow は短く保ち、Win32 呼び出しをまたいで保持しないこと。

struct State {
    cfg: Config,
    history: VecDeque<String>, // 先頭が最新
    ignore_seq: u32,           // 自分が書き込んだクリップボードの連番
    excl_fmt: u32,             // ExcludeClipboardContentFromMonitorProcessing
    folder_hist: VecDeque<String>, // フォルダ履歴 (新しい順)
    cfg_mtime: Option<std::time::SystemTime>, // config.toml の最終更新時刻 (自動再読み込み用)
}

#[derive(PartialEq)]
enum PushKind {
    Ignored,  // 空白のみ、または直前と同じ
    Appended, // 末尾追記だけで済む
    Rewrote,  // 重複の繰り上げ・件数超過 → ファイル全体を書き直す
}

impl State {
    fn push(&mut self, text: String) -> PushKind {
        if text.trim().is_empty() || self.history.front() == Some(&text) {
            return PushKind::Ignored;
        }
        let dup = match self.history.iter().position(|t| *t == text) {
            Some(pos) => {
                self.history.remove(pos);
                true
            }
            None => false,
        };
        self.history.push_front(text);
        let over = self.history.len() > self.cfg.clipboard.max_items;
        self.history.truncate(self.cfg.clipboard.max_items);
        if dup || over {
            PushKind::Rewrote
        } else {
            PushKind::Appended
        }
    }
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|c| c.borrow_mut().as_mut().map(f))
}

/// 履歴に追加し、history.txt にも反映する
fn record(text: String) {
    match with_state(|s| s.push(text.clone())) {
        Some(PushKind::Appended) => append_history_file(&text),
        Some(PushKind::Rewrote) => {
            if let Some(snap) = with_state(|s| s.history.clone()) {
                write_history_file(&snap);
            }
        }
        _ => {}
    }
}

// フックから参照する状態 (フックはメインスレッドで呼ばれるが、念のため atomic)
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
static HAVE_LAST: AtomicBool = AtomicBool::new(false);
static LAST_TIME: AtomicU32 = AtomicU32::new(0);
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);
static HAVE_LAST_L: AtomicBool = AtomicBool::new(false); // 左クリック(デスクトップ用)
static LAST_L_TIME: AtomicU32 = AtomicU32::new(0);
static LAST_L_X: AtomicI32 = AtomicI32::new(0);
static LAST_L_Y: AtomicI32 = AtomicI32::new(0);
static PEND_X: AtomicI32 = AtomicI32::new(0);
static PEND_Y: AtomicI32 = AtomicI32::new(0);
static PEND_HWND: AtomicIsize = AtomicIsize::new(0); // ダブルクリックされたデスクトップ側ウィンドウ
static WAIT_LUP: AtomicBool = AtomicBool::new(false); // 2回目の左ボタンが離されるのを待つ
static SWALLOW_UP: AtomicBool = AtomicBool::new(false);
static FOLDER_MENU_ACTIVE: AtomicBool = AtomicBool::new(false); // フォルダメニュー表示中
static HOT_MENU: AtomicIsize = AtomicIsize::new(0); // 選択中の項目があるメニュー (WM_MENUSELECT)
static HOT_ITEM: AtomicI32 = AtomicI32::new(-1); // 選択中の項目の位置 (サブメニュー項目のときのみ。それ以外は -1)
static SHOWING: AtomicBool = AtomicBool::new(false); // 自分のメニュー表示中はフック無効
static DBL_MS: AtomicU32 = AtomicU32::new(500);
static TB_CREATED: AtomicU32 = AtomicU32::new(0);

// ───────────────────────── ユーティリティ ─────────────────────────

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn error_box(msg: &str) {
    let m = wide(msg);
    unsafe {
        MessageBoxW(HWND::default(), PCWSTR(m.as_ptr()), w!("RClauncher"), MB_OK | MB_ICONERROR);
    }
}

fn apply_dbl_ms(cfg: &Config) {
    let ms = if cfg.general.double_click_ms > 0 {
        cfg.general.double_click_ms
    } else {
        unsafe { GetDoubleClickTime() }
    };
    DBL_MS.store(ms, Relaxed);
}

/// メニュー表示用: 1行にまとめ、PREVIEW_CHARS(10)文字まで。超える場合は末尾に ... を付ける。
/// 改行・タブ・連続空白は空白1つにし、先頭と末尾の空白は無視する。& はエスケープ。
fn preview(s: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut pending_space = false;
    let mut truncated = false;
    for ch in s.chars() {
        if ch.is_whitespace() || ch.is_control() {
            pending_space = count > 0;
            continue;
        }
        if pending_space {
            if count >= PREVIEW_CHARS {
                truncated = true;
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= PREVIEW_CHARS {
            truncated = true; // 11文字目が存在する
            break;
        }
        if ch == '&' {
            out.push_str("&&");
        } else {
            out.push(ch);
        }
        count += 1;
    }
    if truncated {
        out.push_str("...");
    }
    out
}

/// 1-9, A-Z のアクセスキー
fn accel(i: usize) -> Option<char> {
    match i {
        0..=8 => Some((b'1' + i as u8) as char),
        9..=34 => Some((b'A' + (i - 9) as u8) as char),
        _ => None,
    }
}

// ───────────────────────── マウスフック ─────────────────────────

/// ウィンドウのクラス名
unsafe fn class_name(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetClassNameW(h, &mut buf);
    String::from_utf16_lossy(&buf[..(n.max(0) as usize).min(buf.len())])
}

/// 指定座標がデスクトップ(Progman/WorkerW 配下)なら、その座標のウィンドウを返す
unsafe fn desktop_window_at(pt: POINT) -> Option<HWND> {
    let first = WindowFromPoint(pt);
    let mut h = first;
    for _ in 0..6 {
        if h.0.is_null() {
            break;
        }
        let name = class_name(h);
        if name == "Progman" || name == "WorkerW" {
            return Some(first);
        }
        h = GetAncestor(h, GA_PARENT);
    }
    None
}

/// デスクトップ上の「何もない場所」か (アイコン・ショートカット上ではない)
/// Explorer のアイコン一覧(SysListView32)へ LVM_HITTEST を送って判定する。
unsafe fn is_empty_desktop_spot(target: HWND, screen_pt: POINT) -> bool {
    if target.0.is_null() {
        return false;
    }
    if class_name(target) != "SysListView32" {
        return true; // 一覧以外 (壁紙面そのもの) なら空白とみなす
    }
    let mut pt = screen_pt;
    if !ScreenToClient(target, &mut pt).as_bool() {
        return false;
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(target, Some(&mut pid));
    if pid == 0 {
        return false;
    }
    let Ok(proc) = OpenProcess(PROCESS_VM_OPERATION | PROCESS_VM_WRITE, false, pid) else {
        return false;
    };
    let info = LvHitTestInfo { pt, flags: 0, item: -1, sub_item: 0, group: 0 };
    let size = size_of::<LvHitTestInfo>();
    let remote = VirtualAllocEx(proc, None, size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    let mut empty = false;
    if !remote.is_null() {
        if WriteProcessMemory(proc, remote, &info as *const _ as *const c_void, size, None).is_ok() {
            let mut res: usize = 0;
            let ok = SendMessageTimeoutW(
                target,
                LVM_HITTEST,
                WPARAM(0),
                LPARAM(remote as isize),
                SMTO_ABORTIFHUNG,
                300,
                Some(&mut res),
            );
            // 戻り値(項目番号)が -1 なら、どの項目の上でもない
            empty = ok.0 != 0 && (res as i32) == -1;
        }
        let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
    }
    let _ = CloseHandle(proc);
    empty
}

// フック内では軽い処理だけを行う (約300msを超えるとWindowsにフックを外される)
// 重い判定(アイコン上かどうか)は、メインスレッドへPostした後に行う。
unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == 0 && FOLDER_MENU_ACTIVE.load(Relaxed) {
        // フォルダメニュー表示中: 左ボタンが離されたことをメインへ通知するだけ (握りつぶさない)
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.flags & 1 == 0 && wparam.0 as u32 == WM_LBUTTONUP {
            let hwnd = HWND(MAIN_HWND.load(Relaxed) as *mut c_void);
            let _ = PostMessageW(hwnd, WM_APP_MENU_CLICK, WPARAM(0), LPARAM(0));
        }
    }
    if code == 0 && !SHOWING.load(Relaxed) {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.flags & 1 == 0 {
            // LLMHF_INJECTED でない (他ツールの合成入力は無視)
            match wparam.0 as u32 {
                // ── アプリ画面内: 右クリック2回 → クリップボード履歴 ──
                WM_RBUTTONDOWN => {
                    let tol = (GetSystemMetrics(SM_CXDOUBLECLK) / 2).max(2);
                    let second = HAVE_LAST.load(Relaxed)
                        && info.time.wrapping_sub(LAST_TIME.load(Relaxed)) <= DBL_MS.load(Relaxed)
                        && (info.pt.x - LAST_X.load(Relaxed)).abs() <= tol
                        && (info.pt.y - LAST_Y.load(Relaxed)).abs() <= tol;
                    if second {
                        HAVE_LAST.store(false, Relaxed);
                        SWALLOW_UP.store(true, Relaxed);
                        PEND_X.store(info.pt.x, Relaxed);
                        PEND_Y.store(info.pt.y, Relaxed);
                        let hwnd = HWND(MAIN_HWND.load(Relaxed) as *mut c_void);
                        let _ = PostMessageW(hwnd, WM_APP_TRIGGER, WPARAM(0), LPARAM(0));
                        return LRESULT(1); // 2回目は握りつぶす
                    }
                    // 1回目: アプリ本来の右クリックはそのまま通す。
                    // デスクトップ上では右クリック2回は無効 (通常の右クリックのみ)。
                    if desktop_window_at(info.pt).is_some() {
                        HAVE_LAST.store(false, Relaxed);
                    } else {
                        LAST_TIME.store(info.time, Relaxed);
                        LAST_X.store(info.pt.x, Relaxed);
                        LAST_Y.store(info.pt.y, Relaxed);
                        HAVE_LAST.store(true, Relaxed);
                    }
                }
                WM_RBUTTONUP => {
                    if SWALLOW_UP.swap(false, Relaxed) {
                        return LRESULT(1);
                    }
                }
                // ── デスクトップ: ダブルクリック → フォルダ一覧 ──
                // 左クリックは握りつぶさず、傍受するだけ。
                WM_LBUTTONDOWN => {
                    let tol = (GetSystemMetrics(SM_CXDOUBLECLK) / 2).max(2);
                    let second = HAVE_LAST_L.load(Relaxed)
                        && info.time.wrapping_sub(LAST_L_TIME.load(Relaxed)) <= DBL_MS.load(Relaxed)
                        && (info.pt.x - LAST_L_X.load(Relaxed)).abs() <= tol
                        && (info.pt.y - LAST_L_Y.load(Relaxed)).abs() <= tol;
                    if second {
                        HAVE_LAST_L.store(false, Relaxed);
                        if let Some(h) = desktop_window_at(info.pt) {
                            PEND_X.store(info.pt.x, Relaxed);
                            PEND_Y.store(info.pt.y, Relaxed);
                            PEND_HWND.store(h.0 as isize, Relaxed);
                            WAIT_LUP.store(true, Relaxed); // ボタンが離れてからメニューを出す
                        }
                    } else {
                        LAST_L_TIME.store(info.time, Relaxed);
                        LAST_L_X.store(info.pt.x, Relaxed);
                        LAST_L_Y.store(info.pt.y, Relaxed);
                        HAVE_LAST_L.store(true, Relaxed);
                    }
                }
                WM_LBUTTONUP => {
                    if WAIT_LUP.swap(false, Relaxed) {
                        let hwnd = HWND(MAIN_HWND.load(Relaxed) as *mut c_void);
                        let _ = PostMessageW(hwnd, WM_APP_TRIGGER_FOLDER, WPARAM(0), LPARAM(0));
                    }
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(HHOOK::default(), code, wparam, lparam)
}

// ───────────────────────── クリップボード ─────────────────────────

unsafe fn open_clipboard_retry(hwnd: HWND) -> bool {
    for _ in 0..8 {
        if OpenClipboard(hwnd).is_ok() {
            return true;
        }
        Sleep(10); // 他プロセスが一時的に掴んでいることがある
    }
    false
}

unsafe fn read_clipboard_text(hwnd: HWND) -> Option<String> {
    if IsClipboardFormatAvailable(CF_UNICODETEXT).is_err() {
        return None;
    }
    if !open_clipboard_retry(hwnd) {
        return None;
    }
    let mut result = None;
    if let Ok(h) = GetClipboardData(CF_UNICODETEXT) {
        let hg = HGLOBAL(h.0);
        let p = GlobalLock(hg) as *const u16;
        if !p.is_null() {
            let max = GlobalSize(hg) / 2;
            let slice = std::slice::from_raw_parts(p, max);
            let len = slice.iter().position(|&c| c == 0).unwrap_or(max);
            result = Some(String::from_utf16_lossy(&slice[..len]));
            let _ = GlobalUnlock(hg);
        }
    }
    let _ = CloseClipboard();
    result
}

unsafe fn write_clipboard_text(hwnd: HWND, text: &str) -> bool {
    let data = wide(text);
    let bytes = data.len() * 2;
    let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
        return false;
    };
    let p = GlobalLock(hg) as *mut u16;
    if p.is_null() {
        let _ = GlobalFree(hg);
        return false;
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    let _ = GlobalUnlock(hg);

    if !open_clipboard_retry(hwnd) {
        let _ = GlobalFree(hg);
        return false;
    }
    let _ = EmptyClipboard();
    let ok = SetClipboardData(CF_UNICODETEXT, HANDLE(hg.0)).is_ok();
    let _ = CloseClipboard();
    if !ok {
        let _ = GlobalFree(hg); // 成功時は所有権がシステムに移る
    }
    ok
}

unsafe fn on_clipboard_update(hwnd: HWND) {
    let Some((ign, excl)) = with_state(|s| (s.ignore_seq, s.excl_fmt)) else {
        return;
    };
    if GetClipboardSequenceNumber() == ign {
        return; // 自分が書き込んだ分
    }
    if excl != 0 && IsClipboardFormatAvailable(excl).is_ok() {
        return; // パスワードマネージャ等が除外指定したデータ
    }
    if let Some(text) = read_clipboard_text(hwnd) {
        if text.chars().count() <= MAX_ENTRY_CHARS {
            record(text);
        }
    }
}

// ───────────────────────── 入力送信 / フォーカス ─────────────────────────

unsafe fn send_keys(seq: &[(VIRTUAL_KEY, bool)]) {
    let inputs: Vec<INPUT> = seq
        .iter()
        .map(|&(vk, up)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    SendInput(&inputs, size_of::<INPUT>() as i32);
}

/// フォアグラウンドロックを避けて前面化する
unsafe fn force_foreground(target: HWND) {
    let fg = GetForegroundWindow();
    let me = GetCurrentThreadId();
    let fg_tid = if fg.0.is_null() { 0 } else { GetWindowThreadProcessId(fg, None) };
    if fg_tid != 0 && fg_tid != me {
        let _ = AttachThreadInput(me, fg_tid, true);
        let _ = SetForegroundWindow(target);
        let _ = AttachThreadInput(me, fg_tid, false);
    } else {
        let _ = SetForegroundWindow(target);
    }
}

unsafe fn restore_foreground(prev: HWND) {
    if prev.0.is_null() {
        return;
    }
    force_foreground(prev);
    for _ in 0..30 {
        if GetForegroundWindow() == prev {
            break;
        }
        Sleep(10);
    }
}

/// フォアグラウンドのスレッドがメニュー操作中か
unsafe fn foreground_in_menu_mode() -> bool {
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return false;
    }
    let tid = GetWindowThreadProcessId(fg, None);
    let mut gti = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    GetGUIThreadInfo(tid, &mut gti).is_ok() && (gti.flags & GUI_INMENUMODE).0 != 0
}

// ───────────────────────── メニュー ─────────────────────────

unsafe fn append_item(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, label: &str) {
    let l = wide(label);
    let _ = AppendMenuW(menu, flags, id, PCWSTR(l.as_ptr()));
}

unsafe fn build_clip_menu(items: &[String], group: usize) -> HMENU {
    let menu = CreatePopupMenu().unwrap_or_default();
    if items.is_empty() {
        append_item(menu, MF_STRING | MF_GRAYED, 0, "(履歴なし)");
    }
    // 新しい順に group 件までは直接表示。それ以降は group 件ずつサブメニュー。
    for (i, text) in items.iter().take(group).enumerate() {
        let label = match accel(i) {
            Some(c) => format!("&{c}  {}", preview(text)),
            None => format!("    {}", preview(text)),
        };
        append_item(menu, MF_STRING, (CLIP_BASE as usize) + i, &label);
    }
    let mut start = group;
    while start < items.len() {
        let end = (start + group).min(items.len());
        let sub = CreatePopupMenu().unwrap_or_default();
        for (k, text) in items[start..end].iter().enumerate() {
            let label = match accel(k) {
                Some(c) => format!("&{c}  {}", preview(text)),
                None => format!("    {}", preview(text)),
            };
            append_item(sub, MF_STRING, (CLIP_BASE as usize) + start + k, &label);
        }
        append_item(menu, MF_POPUP | MF_STRING, sub.0 as usize, &format!("{} - {}", start + 1, end));
        start = end;
    }
    // 最下段: 履歴クリア
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let flags = if items.is_empty() { MF_STRING | MF_GRAYED } else { MF_STRING };
    append_item(menu, flags, CMD_CLIP_CLEAR as usize, "履歴クリア");
    menu
}

// ── フォルダメニュー (遅延展開) ──
// サブメニューは空のプレースホルダだけ作っておき、開かれた瞬間(WM_INITMENUPOPUP)に
// 中身を列挙する。メニュー表示中もメッセージが届くため、borrow は短く保つこと。

const MAX_TARGETS: usize = 55_000; // メニューID (FOLDER_BASE + n) を 65535 未満に収める

#[derive(Default)]
struct MenuTree {
    pending: HashMap<isize, String>, // サブメニューのHMENU → まだ展開していないフォルダ
    paths: HashMap<isize, String>,   // サブメニューのHMENU → フォルダ (展開後も残す。クリックで開く用)
    targets: Vec<String>,            // メニューID - FOLDER_BASE → 開くフォルダ
}

thread_local! {
    static MENU_TREE: RefCell<MenuTree> = RefCell::new(MenuTree::default());
}

fn reset_menu_tree() {
    MENU_TREE.with(|t| *t.borrow_mut() = MenuTree::default());
}

thread_local! {
    // メニュー上のフォルダ名クリックで開くよう要求されたフォルダ
    static CLICK_OPEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// フォルダ名(サブメニュー付き項目)の上で左クリックされたら、メニューを閉じてそのフォルダを開く要求を出す
unsafe fn on_menu_click(hwnd: HWND) {
    if !FOLDER_MENU_ACTIVE.load(Relaxed) || shellmenu::SHELL_MENU_ACTIVE.load(Relaxed) {
        return;
    }
    let (hm, hi) = (HOT_MENU.load(Relaxed), HOT_ITEM.load(Relaxed));
    if hm == 0 || hi < 0 {
        return;
    }
    let hmenu = HMENU(hm as *mut c_void);
    // クリック位置が、選択中の項目の上であることを確認する (メニュー外のクリックを除く)
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    if MenuItemFromPoint(hwnd, hmenu, pt) != hi {
        return;
    }
    let sub = GetSubMenu(hmenu, hi);
    if sub.0.is_null() {
        return;
    }
    let path = MENU_TREE.with(|t| t.borrow().paths.get(&(sub.0 as isize)).cloned());
    if let Some(path) = path {
        CLICK_OPEN.with(|c| *c.borrow_mut() = Some(path));
        let _ = EndMenu();
    }
}

fn register_pending(menu: HMENU, path: String) {
    MENU_TREE.with(|t| {
        let mut t = t.borrow_mut();
        t.paths.insert(menu.0 as isize, path.clone());
        t.pending.insert(menu.0 as isize, path);
    });
}

fn register_target(path: String) -> Option<usize> {
    MENU_TREE.with(|t| {
        let mut t = t.borrow_mut();
        if t.targets.len() >= MAX_TARGETS {
            return None;
        }
        t.targets.push(path);
        Some(FOLDER_BASE as usize + t.targets.len() - 1)
    })
}

fn lookup_target(id: u32) -> Option<String> {
    let i = id.checked_sub(FOLDER_BASE)? as usize;
    MENU_TREE.with(|t| t.borrow().targets.get(i).cloned())
}

/// %VAR% を環境変数で展開する
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                let key = &after[..j];
                match std::env::var(key) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(key);
                        out.push('%');
                    }
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// ディレクトリ属性か (ジャンクション/シンボリックリンクも含む)。隠し・システムは設定次第で除外。
fn is_listed_dir(md: &std::fs::Metadata, show_hidden: bool) -> bool {
    let attr = md.file_attributes();
    attr & 0x10 != 0 && (show_hidden || attr & 0x6 == 0)
}

fn list_subdirs(path: &str, show_hidden: bool) -> Option<Vec<String>> {
    let rd = std::fs::read_dir(path).ok()?;
    let mut v: Vec<String> = rd
        .flatten()
        .filter(|e| e.metadata().map(|m| is_listed_dir(&m, show_hidden)).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort_by_key(|n| n.to_lowercase());
    Some(v)
}

/// サブフォルダを1つでも持つか (最初に見つかった時点で打ち切る)
fn has_subdir(path: &str, show_hidden: bool) -> bool {
    match std::fs::read_dir(path) {
        Ok(rd) => rd
            .flatten()
            .any(|e| e.metadata().map(|m| is_listed_dir(&m, show_hidden)).unwrap_or(false)),
        Err(_) => false,
    }
}

// ── フォルダ履歴 (exe と同じ場所の folder_history.txt、1行1パス、新しい順) ──

fn folder_hist_path() -> PathBuf {
    exe_dir().join("folder_history.txt")
}

fn load_folder_history() -> VecDeque<String> {
    let Ok(bytes) = std::fs::read(folder_hist_path()) else {
        return VecDeque::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut out: VecDeque<String> = VecDeque::new();
    let mut seen = HashSet::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        let l = line.trim();
        if !l.is_empty() && seen.insert(l.to_lowercase()) {
            out.push_back(l.to_string());
        }
    }
    out
}

fn save_folder_history(h: &VecDeque<String>) {
    let mut buf = String::new();
    for p in h {
        buf.push_str(p);
        buf.push('\n');
    }
    let _ = std::fs::write(folder_hist_path(), buf.as_bytes());
}

fn record_folder(path: &str) {
    let snap = with_state(|s| {
        let max = s.cfg.folder_menu.history_max;
        if max == 0 {
            return None;
        }
        let lower = path.to_lowercase();
        s.folder_hist.retain(|p| p.to_lowercase() != lower);
        s.folder_hist.push_front(path.to_string());
        s.folder_hist.truncate(max);
        Some(s.folder_hist.clone())
    })
    .flatten();
    if let Some(h) = snap {
        save_folder_history(&h);
    }
}

fn remove_folder_history(path: &str) {
    let lower = path.to_lowercase();
    let snap = with_state(|s| {
        s.folder_hist.retain(|p| p.to_lowercase() != lower);
        s.folder_hist.clone()
    });
    if let Some(h) = snap {
        save_folder_history(&h);
    }
}

fn clear_folder_history() {
    with_state(|s| s.folder_hist.clear());
    let _ = std::fs::write(folder_hist_path(), b"");
}

fn short_path(p: &str) -> String {
    let n = p.chars().count();
    let t = if n > 60 {
        let tail: String = p.chars().skip(n - 59).collect();
        format!("…{tail}")
    } else {
        p.to_string()
    };
    t.replace('&', "&&")
}

// ── メニューアイコン ──
// 32bit ARGB の HBITMAP をメニュー項目に付ける。DestroyMenu では解放されないため、
// メニューを閉じた後に free_icons() で破棄する。

struct IconCache {
    generic: Option<HBITMAP>, // 汎用フォルダアイコン (全項目で共有)
    all: Vec<HBITMAP>,        // 破棄対象
}

thread_local! {
    static ICONS: RefCell<IconCache> = RefCell::new(IconCache { generic: None, all: Vec::new() });
}

fn icons_enabled() -> bool {
    with_state(|s| s.cfg.folder_menu.show_icons).unwrap_or(false)
}

unsafe fn icon_to_bitmap(icon: HICON, size: i32) -> Option<HBITMAP> {
    let screen = GetDC(HWND::default());
    let dc = CreateCompatibleDC(screen);
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size,
            biHeight: -size, // 上から下
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let bmp = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0);
    let result = match bmp {
        Ok(hbm) => {
            let old = SelectObject(dc, HGDIOBJ(hbm.0));
            // 透明(0)で初期化された DIB に描くと、アルファ付きアイコンが正しく乗る
            let _ = DrawIconEx(dc, 0, 0, icon, size, size, 0, HBRUSH::default(), DI_NORMAL);
            SelectObject(dc, old);
            Some(hbm)
        }
        Err(_) => None,
    };
    let _ = DeleteDC(dc);
    ReleaseDC(HWND::default(), screen);
    result
}

/// real=false: ディスクに触れず汎用フォルダアイコンを取得 / real=true: そのパス固有のアイコン
unsafe fn bitmap_from_shell(path: &str, real: bool) -> Option<HBITMAP> {
    let size = GetSystemMetrics(SM_CXSMICON).max(16);
    let mut sfi = SHFILEINFOW::default();
    let wpath = wide(path);
    let (attrs, flags) = if real {
        (FILE_FLAGS_AND_ATTRIBUTES(0), SHGFI_ICON | SHGFI_SMALLICON)
    } else {
        (FILE_ATTRIBUTE_DIRECTORY, SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES)
    };
    let r = SHGetFileInfoW(
        PCWSTR(wpath.as_ptr()),
        attrs,
        Some(&mut sfi as *mut SHFILEINFOW),
        size_of::<SHFILEINFOW>() as u32,
        flags,
    );
    if r == 0 || sfi.hIcon.0.is_null() {
        return None;
    }
    let bmp = icon_to_bitmap(sfi.hIcon, size);
    let _ = DestroyIcon(sfi.hIcon);
    bmp
}

fn track_bitmap(b: HBITMAP) {
    ICONS.with(|c| c.borrow_mut().all.push(b));
}

unsafe fn generic_folder_bitmap() -> Option<HBITMAP> {
    if !icons_enabled() {
        return None;
    }
    if let Some(b) = ICONS.with(|c| c.borrow().generic) {
        return Some(b);
    }
    let b = bitmap_from_shell("folder", false)?;
    track_bitmap(b);
    ICONS.with(|c| c.borrow_mut().generic = Some(b));
    Some(b)
}

/// 登録フォルダ用: そのフォルダ固有のアイコン (取得できなければ汎用)
unsafe fn real_folder_bitmap(path: &str) -> Option<HBITMAP> {
    if !icons_enabled() {
        return None;
    }
    match bitmap_from_shell(path, true) {
        Some(b) => {
            track_bitmap(b);
            Some(b)
        }
        None => generic_folder_bitmap(),
    }
}

unsafe fn free_icons() {
    ICONS.with(|c| {
        let mut c = c.borrow_mut();
        for b in c.all.drain(..) {
            let _ = DeleteObject(HGDIOBJ(b.0));
        }
        c.generic = None;
    });
}

/// 直前に追加した項目へビットマップを付ける
unsafe fn set_last_item_bitmap(menu: HMENU, bmp: Option<HBITMAP>) {
    let Some(bmp) = bmp else { return };
    let n = GetMenuItemCount(menu);
    if n <= 0 {
        return;
    }
    let mii = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_BITMAP,
        hbmpItem: bmp,
        ..Default::default()
    };
    let _ = SetMenuItemInfoW(menu, (n - 1) as u32, true, &mii);
}

unsafe fn append_item_icon(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, label: &str, bmp: Option<HBITMAP>) {
    append_item(menu, flags, id, label);
    set_last_item_bitmap(menu, bmp);
}

unsafe fn append_placeholder_submenu(parent: HMENU, path: String, label: &str, bmp: Option<HBITMAP>) {
    let sub = CreatePopupMenu().unwrap_or_default();
    append_item(sub, MF_STRING | MF_GRAYED, 0, "…"); // 空だと開かないことがあるため
    register_pending(sub, path);
    append_item_icon(parent, MF_POPUP | MF_STRING, sub.0 as usize, &label.replace('&', "&&"), bmp);
}

unsafe fn build_folder_history_menu(hist: &[String]) -> HMENU {
    let sub = CreatePopupMenu().unwrap_or_default();
    let generic = generic_folder_bitmap();
    if hist.is_empty() {
        append_item(sub, MF_STRING | MF_GRAYED, 0, "(履歴なし)");
    }
    for p in hist {
        if let Some(id) = register_target(p.clone()) {
            append_item_icon(sub, MF_STRING, id, &short_path(p), generic);
        }
    }
    let _ = AppendMenuW(sub, MF_SEPARATOR, 0, PCWSTR::null());
    let flags = if hist.is_empty() { MF_STRING | MF_GRAYED } else { MF_STRING };
    append_item(sub, flags, CMD_FHIST_CLEAR as usize, "履歴クリア");
    sub
}

unsafe fn build_folder_menu(folders: &[Folder], hist: &[String], hist_on: bool) -> HMENU {
    let menu = CreatePopupMenu().unwrap_or_default();
    if hist_on {
        let sub = build_folder_history_menu(hist);
        append_item_icon(menu, MF_POPUP | MF_STRING, sub.0 as usize, "履歴", generic_folder_bitmap());
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
    if folders.is_empty() {
        append_item(menu, MF_STRING | MF_GRAYED, 0, "(config.toml に [[folders]] を追加してください)");
        return menu;
    }
    for f in folders {
        let path = expand_env(&f.path);
        let name = if f.name.is_empty() {
            std::path::Path::new(&path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone())
        } else {
            f.name.clone()
        };
        let icon = real_folder_bitmap(&path);
        append_placeholder_submenu(menu, path, &name, icon);
    }
    menu
}

/// WM_INITMENUPOPUP: サブメニューが開かれる直前に中身を作る
unsafe fn on_init_menu_popup(menu: HMENU) {
    let Some(path) = MENU_TREE.with(|t| t.borrow_mut().pending.remove(&(menu.0 as isize))) else {
        return; // 展開対象でないメニュー、または展開済み
    };
    let (max, hidden) = with_state(|s| (s.cfg.folder_menu.max_entries, s.cfg.folder_menu.show_hidden))
        .unwrap_or((300, false));
    let generic = generic_folder_bitmap();

    let _ = DeleteMenu(menu, 0, MF_BYPOSITION); // プレースホルダを除去

    if !std::path::Path::new(&path).is_dir() {
        append_item(menu, MF_STRING | MF_GRAYED, 0, "(フォルダが見つかりません)");
        return;
    }
    if let Some(id) = register_target(path.clone()) {
        append_item_icon(menu, MF_STRING, id, "このフォルダを開く", generic);
    }
    let Some(names) = list_subdirs(&path, hidden) else {
        append_item(menu, MF_STRING | MF_GRAYED, 0, "(中身を読み取れません)");
        return;
    };
    if names.is_empty() {
        return;
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    for name in names.iter().take(max) {
        let full = std::path::Path::new(&path).join(name).to_string_lossy().into_owned();
        if has_subdir(&full, hidden) {
            append_placeholder_submenu(menu, full, name, generic);
        } else if let Some(id) = register_target(full) {
            append_item_icon(menu, MF_STRING, id, &name.replace('&', "&&"), generic);
        }
    }
    if names.len() > max {
        append_item(menu, MF_STRING | MF_GRAYED, 0, &format!("… 他 {} 件", names.len() - max));
    }
}

/// メニューを表示して選択IDを返す (キャンセル時 None)
unsafe fn track_ex(hwnd: HWND, menu: HMENU, x: i32, y: i32, flags: TRACK_POPUP_MENU_FLAGS) -> Option<u32> {
    force_foreground(hwnd); // これをしないとメニュー外クリックで閉じない
    let r = TrackPopupMenu(menu, flags, x, y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    if r.0 > 0 {
        Some(r.0 as u32)
    } else {
        None
    }
}

/// 通常のメニュー (通知メッセージ不要)
unsafe fn track(hwnd: HWND, menu: HMENU, x: i32, y: i32) -> Option<u32> {
    track_ex(hwnd, menu, x, y, TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON)
}

/// クリップボード履歴メニュー (アプリ画面内の右クリック2回)
unsafe fn show_clip_popup(hwnd: HWND) {
    let prev = GetForegroundWindow();
    let (x, y) = (PEND_X.load(Relaxed), PEND_Y.load(Relaxed));

    // history.txt を参照して、最新の履歴を読み込む
    sync_history_from_file();
    let snap = with_state(|s| {
        (s.history.iter().cloned().collect::<Vec<String>>(), s.cfg.clipboard.group_size)
    });
    let (items, group) = snap.unwrap_or_default();

    SHOWING.store(true, Relaxed);
    let menu = build_clip_menu(&items, group);
    let id = track(hwnd, menu, x, y);
    let _ = DestroyMenu(menu);
    SHOWING.store(false, Relaxed);
    HAVE_LAST.store(false, Relaxed);
    HAVE_LAST_L.store(false, Relaxed);

    // 選択有無にかかわらず元のウィンドウへフォーカスを戻す
    restore_foreground(prev);

    match id {
        Some(CMD_CLIP_CLEAR) => clear_history(),
        Some(id) => {
            let text = id.checked_sub(CLIP_BASE).and_then(|i| items.get(i as usize).cloned());
            if let Some(text) = text {
                paste_text(hwnd, &text);
            }
        }
        None => {}
    }
}

/// フォルダ一覧メニュー (デスクトップの何もない場所でダブルクリック)
unsafe fn show_folder_popup(hwnd: HWND) {
    let (x, y) = (PEND_X.load(Relaxed), PEND_Y.load(Relaxed));
    let target = HWND(PEND_HWND.load(Relaxed) as *mut c_void);
    // アイコン/ショートカット上のダブルクリックなら何もしない
    if !is_empty_desktop_spot(target, POINT { x, y }) {
        return;
    }
    let prev = GetForegroundWindow();
    let snap = with_state(|s| {
        let max = s.cfg.folder_menu.history_max;
        (s.cfg.folders.clone(), s.folder_hist.iter().take(max).cloned().collect::<Vec<String>>(), max > 0)
    });
    let (folders, hist, hist_on) = snap.unwrap_or_default();

    SHOWING.store(true, Relaxed);
    reset_menu_tree();
    CLICK_OPEN.with(|c| *c.borrow_mut() = None);
    HOT_MENU.store(0, Relaxed);
    HOT_ITEM.store(-1, Relaxed);
    let menu = build_folder_menu(&folders, &hist, hist_on);
    // サブメニューの遅延展開(WM_INITMENUPOPUP)と選択追跡(WM_MENUSELECT)のため TPM_NONOTIFY は付けない
    FOLDER_MENU_ACTIVE.store(true, Relaxed);
    let id = track_ex(hwnd, menu, x, y, TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN | TPM_RIGHTBUTTON);
    FOLDER_MENU_ACTIVE.store(false, Relaxed);
    let _ = DestroyMenu(menu); // 遅延生成したサブメニューも一緒に破棄される
    free_icons(); // メニュー用ビットマップは別途破棄が必要
    SHOWING.store(false, Relaxed);
    HAVE_LAST.store(false, Relaxed);
    HAVE_LAST_L.store(false, Relaxed);

    // 通常の項目の選択 → そのフォルダ。フォルダ名(サブメニュー付き項目)のクリック → CLICK_OPEN。
    let clicked = CLICK_OPEN.with(|c| c.borrow_mut().take());
    let path = id.and_then(lookup_target).or(clicked);
    reset_menu_tree();

    restore_foreground(prev);

    if id == Some(CMD_FHIST_CLEAR) {
        clear_folder_history();
    } else if let Some(path) = path {
        open_folder_checked(&path);
    }
}

/// フォルダを開いて履歴に記録する。存在しなければ履歴から外して通知する。
unsafe fn open_folder_checked(path: &str) {
    if !std::path::Path::new(path).is_dir() {
        remove_folder_history(path);
        error_box(&format!("フォルダが見つかりません:\n{path}"));
        return;
    }
    open_path(path);
    record_folder(path);
}

unsafe fn paste_text(hwnd: HWND, text: &str) {
    if !write_clipboard_text(hwnd, text) {
        return;
    }
    let seq = GetClipboardSequenceNumber();
    with_state(|s| s.ignore_seq = seq);
    record(text.to_string()); // 使った項目を先頭へ
    Sleep(20);
    send_keys(&[(VK_CONTROL, false), (VK_V, false), (VK_V, true), (VK_CONTROL, true)]);
}

unsafe fn open_path(path: &str) {
    let p = wide(path);
    ShellExecuteW(
        HWND::default(),
        w!("open"),
        PCWSTR(p.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn on_trigger(hwnd: HWND) {
    if foreground_in_menu_mode() {
        // 1回目の右クリックで開いたアプリ側メニューを閉じてから自前メニューを出す
        send_keys(&[(VK_ESCAPE, false), (VK_ESCAPE, true)]);
        SetTimer(hwnd, TIMER_SHOW, 60, None);
    } else {
        show_clip_popup(hwnd);
    }
}

// ───────────────────────── 設定の再読み込み ─────────────────────────

fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

/// config.toml を読み直して反映する (エラー時はメッセージ表示。設定は変更しない)
fn reload_config() {
    // 先に更新時刻を記録する (エラーのファイルで毎回メッセージを出さないため)
    let mtime = config_mtime();
    with_state(|s| s.cfg_mtime = mtime);
    match load_config() {
        Ok(cfg) => {
            apply_dbl_ms(&cfg);
            with_state(|s| {
                s.cfg = cfg;
                let m = s.cfg.clipboard.max_items;
                s.history.truncate(m);
            });
        }
        Err(e) => error_box(&e),
    }
}

/// 2秒ごとに呼ばれ、config.toml が更新されていれば再読み込みする
fn check_config_changed() {
    if SHOWING.load(Relaxed) {
        return; // メニュー表示中は避ける
    }
    let cur = config_mtime();
    if with_state(|s| s.cfg_mtime != cur).unwrap_or(false) {
        reload_config();
    }
}

unsafe fn open_in_notepad(path: &std::path::Path) {
    let arg = wide(&format!("\"{}\"", path.display()));
    ShellExecuteW(
        HWND::default(),
        w!("open"),
        w!("notepad.exe"),
        PCWSTR(arg.as_ptr()),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

// ───────────────────────── タスクトレイ ─────────────────────────

unsafe fn add_tray(hwnd: HWND) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: LoadIconW(HINSTANCE::default(), IDI_APPLICATION).unwrap_or_default(),
        ..Default::default()
    };
    let tip: Vec<u16> = "RClauncher".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let auto = settings::autostart_enabled();
    let menu = CreatePopupMenu().unwrap_or_default();
    append_item(menu, MF_STRING, CMD_SETTINGS as usize, "設定(&S)...");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, MF_STRING, CMD_OPEN_CFG as usize, "設定ファイルを開く(&F)");
    append_item(menu, MF_STRING, CMD_OPEN_CFG_DIR as usize, "設定フォルダを開く(&D)");
    append_item(menu, MF_STRING, CMD_RELOAD as usize, "設定を再読み込み(&R)");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let auto_flags = if auto { MF_STRING | MF_CHECKED } else { MF_STRING };
    append_item(menu, auto_flags, CMD_AUTOSTART as usize, "Windows 起動時に自動起動(&A)");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append_item(menu, MF_STRING, CMD_EXIT as usize, "終了(&X)");

    SHOWING.store(true, Relaxed);
    let id = track(hwnd, menu, pt.x, pt.y);
    SHOWING.store(false, Relaxed);
    let _ = DestroyMenu(menu);
    match id {
        Some(CMD_SETTINGS) => settings::open_settings(),
        Some(CMD_OPEN_CFG) => open_in_notepad(&config_path()),
        Some(CMD_OPEN_CFG_DIR) => open_path(&exe_dir().to_string_lossy()),
        Some(CMD_RELOAD) => reload_config(),
        Some(CMD_AUTOSTART) => settings::set_autostart(!auto),
        Some(CMD_EXIT) => {
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

// ───────────────────────── ウィンドウ ─────────────────────────

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Explorer のコンテキストメニュー表示中は、関連メッセージをシェルへ転送する
    if let Some(r) = shellmenu::forward_menu_msg(msg, wparam, lparam) {
        return r;
    }
    match msg {
        WM_CLIPBOARDUPDATE => {
            on_clipboard_update(hwnd);
            LRESULT(0)
        }
        WM_APP_TRIGGER => {
            on_trigger(hwnd);
            LRESULT(0)
        }
        WM_APP_TRIGGER_FOLDER => {
            show_folder_popup(hwnd);
            LRESULT(0)
        }
        WM_INITMENUPOPUP => {
            on_init_menu_popup(HMENU(wparam.0 as *mut c_void));
            LRESULT(0)
        }
        WM_MENUSELECT => {
            // 下位ワード: 項目の位置(サブメニューのとき) / 上位ワード: フラグ (0xFFFF = メニューが閉じた)
            let item = (wparam.0 & 0xFFFF) as i32;
            let flags = ((wparam.0 >> 16) & 0xFFFF) as u32;
            HOT_MENU.store(lparam.0, Relaxed);
            let is_popup = lparam.0 != 0 && flags != 0xFFFF && flags & 0x10 != 0; // MF_POPUP
            HOT_ITEM.store(if is_popup { item } else { -1 }, Relaxed);
            LRESULT(0)
        }
        WM_APP_MENU_CLICK => {
            on_menu_click(hwnd);
            LRESULT(0)
        }
        WM_MENURBUTTONUP => {
            // フォルダ項目の右クリック → Explorer のコンテキストメニュー
            shellmenu::on_menu_rbutton(hwnd, wparam.0 as u32, HMENU(lparam.0 as *mut c_void));
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_SHOW {
                let _ = KillTimer(hwnd, TIMER_SHOW);
                show_clip_popup(hwnd);
            } else if wparam.0 == TIMER_CFG {
                check_config_changed();
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            if (lparam.0 as u32 & 0xFFFF) == WM_RBUTTONUP || (lparam.0 as u32 & 0xFFFF) == WM_LBUTTONUP {
                show_tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = RemoveClipboardFormatListener(hwnd);
            remove_tray(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            if msg != 0 && msg == TB_CREATED.load(Relaxed) {
                add_tray(hwnd); // Explorer 再起動後にアイコンを再登録
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

unsafe fn run() -> windows::core::Result<()> {
    // 高DPI対応 (フックの座標と、メニューアイコンのサイズを物理ピクセルで扱う)
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    // フォルダ選択ダイアログ・Explorer のメニュー(COM)に必要
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    // 多重起動防止
    let _mutex = CreateMutexW(None, true, w!("Local\\RClauncher_SingleInstance"))?;
    if GetLastError() == ERROR_ALREADY_EXISTS {
        return Ok(());
    }

    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error_box(&e);
            Config::default()
        }
    };
    apply_dbl_ms(&cfg);
    let excl_fmt = RegisterClipboardFormatW(w!("ExcludeClipboardContentFromMonitorProcessing"));
    STATE.with(|c| {
        *c.borrow_mut() = Some(State { cfg, history: VecDeque::new(), ignore_seq: 0, excl_fmt, folder_hist: load_folder_history(), cfg_mtime: config_mtime() });
    });

    // clipboard フォルダが無ければ作成し、履歴を読み込む
    ensure_clipboard_dir();
    sync_history_from_file();

    let hmod = GetModuleHandleW(PCWSTR::null())?;
    let hinst = HINSTANCE(hmod.0);
    let class = w!("RClauncherMsgWnd");
    let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: class, ..Default::default() };
    RegisterClassW(&wc);

    // メッセージ専用ウィンドウ (画面には出ない)
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class,
        w!("RClauncher"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        HWND_MESSAGE,
        HMENU::default(),
        hinst,
        None,
    )?;
    MAIN_HWND.store(hwnd.0 as isize, Relaxed);

    TB_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Relaxed);
    AddClipboardFormatListener(hwnd)?;
    // 起動時点のクリップボード内容も履歴に入れる
    on_clipboard_update(hwnd);
    add_tray(hwnd);
    SetTimer(hwnd, TIMER_CFG, 2000, None);

    let hook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinst, 0)?;

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
        // 設定ダイアログが開いていれば Tab/矢印キー等のダイアログ操作を処理させる
        let dlg = settings::settings_hwnd();
        if !dlg.0.is_null() && IsDialogMessageW(dlg, &mut msg).as_bool() {
            continue;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }

    let _ = UnhookWindowsHookEx(hook);
    Ok(())
}

fn main() {
    if let Err(e) = unsafe { run() } {
        error_box(&format!("起動に失敗しました: {e}"));
    }
}
