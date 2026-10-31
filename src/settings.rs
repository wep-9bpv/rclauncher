//! 設定ダイアログ (トレイメニュー → 設定)
//! リソースファイルを使わず、ウィンドウとコントロールをコードで作る。
//! 保存時は toml_edit で config.toml を更新するので、既存のコメントはできるだけ保たれる。

use std::collections::HashMap;

use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};
use windows::core::IUnknown;
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HFONT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForSystem, SystemParametersInfoForDpi};

use super::*;

// ───────────────────────── 定数 ─────────────────────────

const IDC_LIST: i32 = 101;
const IDC_NAME: i32 = 102;
const IDC_PATH: i32 = 103;
const IDC_BROWSE: i32 = 104;
const IDC_ADD: i32 = 105;
const IDC_UPDATE: i32 = 106;
const IDC_DEL: i32 = 107;
const IDC_UP: i32 = 108;
const IDC_DOWN: i32 = 109;
const IDC_DBL: i32 = 110;
const IDC_CLIPMAX: i32 = 111;
const IDC_GROUP: i32 = 112;
const IDC_HISTMAX: i32 = 113;
const IDC_MAXENT: i32 = 114;
const IDC_HIDDEN: i32 = 115;
const IDC_ICONS: i32 = 116;
const IDC_AUTOSTART: i32 = 117;
// IDOK(1)/IDCANCEL(2) を使うと、Enter キーで保存されてしまうため別IDにする
const IDC_SAVE: i32 = 201;
const IDC_CANCEL_BTN: i32 = 202;

const BS_PUSHBUTTON_: u32 = 0;
const BS_AUTOCHECKBOX_: u32 = 3;
const ES_AUTOHSCROLL_: u32 = 0x80;
const LBS_NOTIFY_: u32 = 1;
const LB_ADDSTRING_: u32 = 0x0180;
const LB_RESETCONTENT_: u32 = 0x0184;
const LB_SETCURSEL_: u32 = 0x0186;
const LB_GETCURSEL_: u32 = 0x0188;
const BM_GETCHECK_: u32 = 0x00F0;
const BM_SETCHECK_: u32 = 0x00F1;
const LBN_SELCHANGE_: u32 = 1;

const RUN_VALUE_NAME: PCWSTR = w!("RClauncher");

// ───────────────────────── 状態 ─────────────────────────

struct Dlg {
    hwnd: HWND,
    font: HFONT,
    folders: Vec<Folder>,
    ctl: HashMap<i32, HWND>,
}

thread_local! {
    static DLG: RefCell<Option<Dlg>> = const { RefCell::new(None) };
}

/// 設定ダイアログが開いていればそのハンドル (無ければ null)
pub fn settings_hwnd() -> HWND {
    DLG.with(|d| d.borrow().as_ref().map(|d| d.hwnd)).unwrap_or_default()
}

fn ctl(id: i32) -> HWND {
    DLG.with(|d| d.borrow().as_ref().and_then(|d| d.ctl.get(&id).copied())).unwrap_or_default()
}

fn sc(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

// ───────────────────────── 自動起動 (レジストリ Run キー) ─────────────────────────

pub unsafe fn autostart_enabled() -> bool {
    let mut hkey = HKEY::default();
    if RegOpenKeyExW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
        0,
        KEY_QUERY_VALUE,
        &mut hkey,
    ) != ERROR_SUCCESS
    {
        return false;
    }
    let r = RegQueryValueExW(hkey, RUN_VALUE_NAME, None, None, None, None);
    let _ = RegCloseKey(hkey);
    r == ERROR_SUCCESS
}

pub unsafe fn set_autostart(on: bool) {
    let mut hkey = HKEY::default();
    if RegOpenKeyExW(
        HKEY_CURRENT_USER,
        w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
        0,
        KEY_SET_VALUE,
        &mut hkey,
    ) != ERROR_SUCCESS
    {
        return;
    }
    if on {
        if let Ok(exe) = std::env::current_exe() {
            let cmd = wide(&format!("\"{}\"", exe.display()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2);
            let _ = RegSetValueExW(hkey, RUN_VALUE_NAME, 0, REG_SZ, Some(bytes));
        }
    } else {
        let _ = RegDeleteValueW(hkey, RUN_VALUE_NAME);
    }
    let _ = RegCloseKey(hkey);
}

// ───────────────────────── config.toml の保存 ─────────────────────────

fn set_val(doc: &mut DocumentMut, table: &str, key: &str, v: Item) {
    if !doc.contains_key(table) || !doc[table].is_table() {
        doc[table] = Item::Table(Table::new());
    }
    doc[table][key] = v;
}

struct Values {
    double_click_ms: usize,
    max_items: usize,
    group_size: usize,
    history_max: usize,
    max_entries: usize,
    show_hidden: bool,
    show_icons: bool,
    folders: Vec<Folder>,
}

fn save_config_file(v: &Values) -> Result<(), String> {
    let path = config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_CONFIG.to_string());
    let mut doc = text
        .trim_start_matches('\u{feff}')
        .parse::<DocumentMut>()
        .map_err(|e| format!("現在の config.toml を解析できないため保存できません:\n{e}"))?;

    set_val(&mut doc, "general", "double_click_ms", value(v.double_click_ms as i64));
    set_val(&mut doc, "clipboard", "max_items", value(v.max_items as i64));
    set_val(&mut doc, "clipboard", "group_size", value(v.group_size as i64));
    set_val(&mut doc, "folder_menu", "max_entries", value(v.max_entries as i64));
    set_val(&mut doc, "folder_menu", "show_hidden", value(v.show_hidden));
    set_val(&mut doc, "folder_menu", "history_max", value(v.history_max as i64));
    set_val(&mut doc, "folder_menu", "show_icons", value(v.show_icons));

    let mut arr = ArrayOfTables::new();
    for f in &v.folders {
        let mut t = Table::new();
        if !f.name.is_empty() {
            t["name"] = value(f.name.clone());
        }
        t["path"] = value(f.path.clone());
        arr.push(t);
    }
    doc["folders"] = Item::ArrayOfTables(arr);

    std::fs::write(&path, doc.to_string()).map_err(|e| format!("config.toml の保存に失敗しました:\n{e}"))
}

// ───────────────────────── コントロール操作 ─────────────────────────

unsafe fn get_text(h: HWND) -> String {
    let len = GetWindowTextLengthW(h);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(h, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
}

unsafe fn set_check(h: HWND, on: bool) {
    SendMessageW(h, BM_SETCHECK_, WPARAM(on as usize), LPARAM(0));
}

unsafe fn is_checked(h: HWND) -> bool {
    SendMessageW(h, BM_GETCHECK_, WPARAM(0), LPARAM(0)).0 == 1
}

unsafe fn selected() -> i32 {
    SendMessageW(ctl(IDC_LIST), LB_GETCURSEL_, WPARAM(0), LPARAM(0)).0 as i32
}

unsafe fn refresh_list(select: i32) {
    let Some((list, folders)) = DLG.with(|d| {
        d.borrow().as_ref().map(|d| (d.ctl.get(&IDC_LIST).copied().unwrap_or_default(), d.folders.clone()))
    }) else {
        return;
    };
    SendMessageW(list, LB_RESETCONTENT_, WPARAM(0), LPARAM(0));
    for f in &folders {
        let label = if f.name.is_empty() { f.path.clone() } else { format!("{}  ―  {}", f.name, f.path) };
        let w = wide(&label);
        SendMessageW(list, LB_ADDSTRING_, WPARAM(0), LPARAM(w.as_ptr() as isize));
    }
    if select >= 0 && (select as usize) < folders.len() {
        SendMessageW(list, LB_SETCURSEL_, WPARAM(select as usize), LPARAM(0));
    }
}

unsafe fn load_selected() {
    let idx = selected();
    let f = DLG.with(|d| d.borrow().as_ref().and_then(|d| d.folders.get(idx.max(0) as usize).cloned()));
    if idx < 0 {
        return;
    }
    if let Some(f) = f {
        set_text(ctl(IDC_NAME), &f.name);
        set_text(ctl(IDC_PATH), &f.path);
    }
}

unsafe fn add_or_update(update: bool) {
    let name = get_text(ctl(IDC_NAME)).trim().to_string();
    let path = get_text(ctl(IDC_PATH)).trim().to_string();
    if path.is_empty() {
        return;
    }
    let sel = selected();
    let new_sel = DLG.with(|d| {
        let mut d = d.borrow_mut();
        let d = d.as_mut()?;
        if update {
            if sel < 0 || sel as usize >= d.folders.len() {
                return None;
            }
            d.folders[sel as usize] = Folder { name, path };
            Some(sel)
        } else {
            d.folders.push(Folder { name, path });
            Some(d.folders.len() as i32 - 1)
        }
    });
    if let Some(i) = new_sel {
        refresh_list(i);
        if !update {
            set_text(ctl(IDC_NAME), "");
            set_text(ctl(IDC_PATH), "");
        }
    }
}

unsafe fn delete_selected() {
    let sel = selected();
    if sel < 0 {
        return;
    }
    let len = DLG
        .with(|d| {
            let mut d = d.borrow_mut();
            let d = d.as_mut()?;
            if (sel as usize) < d.folders.len() {
                d.folders.remove(sel as usize);
            }
            Some(d.folders.len() as i32)
        })
        .unwrap_or(0);
    refresh_list(sel.min(len - 1));
    load_selected();
}

unsafe fn move_selected(delta: i32) {
    let sel = selected();
    let target = sel + delta;
    if sel < 0 || target < 0 {
        return;
    }
    let moved = DLG
        .with(|d| {
            let mut d = d.borrow_mut();
            let d = d.as_mut()?;
            if (target as usize) >= d.folders.len() {
                return None;
            }
            d.folders.swap(sel as usize, target as usize);
            Some(())
        })
        .is_some();
    if moved {
        refresh_list(target);
    }
}

/// フォルダ選択ダイアログ (標準の IFileOpenDialog)
unsafe fn pick_folder(owner: HWND) -> Option<String> {
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None::<&IUnknown>, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(opts | FOS_PICKFOLDERS).ok()?;
    dlg.Show(owner).ok()?;
    let item = dlg.GetResult().ok()?;
    let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = name.to_string().ok();
    CoTaskMemFree(Some(name.0 as *const c_void));
    s
}

unsafe fn browse(owner: HWND) {
    if let Some(p) = pick_folder(owner) {
        set_text(ctl(IDC_PATH), &p);
        if get_text(ctl(IDC_NAME)).trim().is_empty() {
            if let Some(n) = std::path::Path::new(&p).file_name() {
                set_text(ctl(IDC_NAME), &n.to_string_lossy());
            }
        }
    }
}

unsafe fn save(hwnd: HWND) {
    let num = |id: i32, lo: usize, hi: usize, label: &str| -> Result<usize, String> {
        match get_text(ctl(id)).trim().parse::<usize>() {
            Ok(v) if v >= lo && v <= hi => Ok(v),
            _ => Err(format!("「{label}」は {lo}〜{hi} の整数で指定してください。")),
        }
    };
    macro_rules! n {
        ($id:expr, $lo:expr, $hi:expr, $label:expr) => {
            match num($id, $lo, $hi, $label) {
                Ok(v) => v,
                Err(e) => {
                    error_box(&e);
                    return;
                }
            }
        };
    }
    let values = Values {
        double_click_ms: n!(IDC_DBL, 0, 5000, "ダブルクリック間隔"),
        max_items: n!(IDC_CLIPMAX, 1, 500, "クリップボード履歴の最大件数"),
        group_size: n!(IDC_GROUP, 1, 50, "クリップボードの直接表示件数"),
        history_max: n!(IDC_HISTMAX, 0, 100, "フォルダ履歴の最大件数"),
        max_entries: n!(IDC_MAXENT, 10, 1000, "サブメニューの最大項目数"),
        show_hidden: is_checked(ctl(IDC_HIDDEN)),
        show_icons: is_checked(ctl(IDC_ICONS)),
        folders: DLG.with(|d| d.borrow().as_ref().map(|d| d.folders.clone())).unwrap_or_default(),
    };
    let autostart = is_checked(ctl(IDC_AUTOSTART));

    if let Err(e) = save_config_file(&values) {
        error_box(&e);
        return;
    }
    reload_config();
    set_autostart(autostart);
    let _ = DestroyWindow(hwnd);
}

unsafe fn on_command(hwnd: HWND, id: i32, code: u32) {
    match id {
        IDC_LIST if code == LBN_SELCHANGE_ => load_selected(),
        IDC_BROWSE => browse(hwnd),
        IDC_ADD => add_or_update(false),
        IDC_UPDATE => add_or_update(true),
        IDC_DEL => delete_selected(),
        IDC_UP => move_selected(-1),
        IDC_DOWN => move_selected(1),
        IDC_SAVE => save(hwnd),
        IDC_CANCEL_BTN | 2 => {
            // 2 = IDCANCEL (Esc キー)。1 = IDOK (Enter キー) は無視する
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            on_command(hwnd, (wparam.0 & 0xFFFF) as i32, ((wparam.0 >> 16) & 0xFFFF) as u32);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // 注意: メインのメッセージループは止めない (PostQuitMessage しない)
            if let Some(d) = DLG.with(|d| d.borrow_mut().take()) {
                let _ = DeleteObject(HGDIOBJ(d.font.0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ───────────────────────── ウィンドウ作成 ─────────────────────────

unsafe fn make_font(dpi: u32) -> HFONT {
    let mut ncm = NONCLIENTMETRICSW { cbSize: size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
    if SystemParametersInfoForDpi(
        SPI_GETNONCLIENTMETRICS.0,
        ncm.cbSize,
        Some(&mut ncm as *mut NONCLIENTMETRICSW as *mut c_void),
        0,
        dpi,
    )
    .is_ok()
    {
        return CreateFontIndirectW(&ncm.lfMessageFont);
    }
    HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
}

#[allow(clippy::too_many_arguments)]
unsafe fn mk(
    parent: HWND,
    hinst: HINSTANCE,
    class: PCWSTR,
    text: &str,
    style: u32,
    ex: u32,
    r: (i32, i32, i32, i32),
    id: i32,
    font: HFONT,
    dpi: u32,
) -> HWND {
    let t = wide(text);
    let hw = CreateWindowExW(
        WINDOW_EX_STYLE(ex),
        class,
        PCWSTR(t.as_ptr()),
        WINDOW_STYLE(style | WS_CHILD.0 | WS_VISIBLE.0),
        sc(r.0, dpi),
        sc(r.1, dpi),
        sc(r.2, dpi),
        sc(r.3, dpi),
        parent,
        HMENU(id as isize as *mut c_void),
        hinst,
        None,
    )
    .unwrap_or_default();
    SendMessageW(hw, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
    hw
}

/// 設定ダイアログを開く (既に開いていれば前面に出す)
pub unsafe fn open_settings() {
    let existing = settings_hwnd();
    if !existing.0.is_null() {
        let _ = ShowWindow(existing, SW_SHOWNORMAL);
        force_foreground(existing);
        return;
    }
    let hmod = GetModuleHandleW(PCWSTR::null()).unwrap_or_default();
    let hinst = HINSTANCE(hmod.0);
    let class = w!("RClauncherSettings");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(settings_proc),
        hInstance: hinst,
        lpszClassName: class,
        hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut c_void),
        hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc); // 2回目以降は失敗するが問題なし

    let dpi = GetDpiForSystem();
    let (cw, ch) = (560, 470);
    let style = WS_CAPTION | WS_SYSMENU;
    let mut rc = RECT { left: 0, top: 0, right: sc(cw, dpi), bottom: sc(ch, dpi) };
    let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dpi);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let x = ((GetSystemMetrics(SM_CXSCREEN) - w) / 2).max(0);
    let y = ((GetSystemMetrics(SM_CYSCREEN) - h) / 2).max(0);
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        w!("RClauncher 設定"),
        style,
        x,
        y,
        w,
        h,
        HWND::default(),
        HMENU::default(),
        hinst,
        None,
    ) else {
        return;
    };

    let font = make_font(dpi);
    let tab = WS_TABSTOP.0;
    let edge = WS_EX_CLIENTEDGE.0;
    let mut c: HashMap<i32, HWND> = HashMap::new();
    macro_rules! ctl_new {
        ($id:expr, $class:expr, $text:expr, $style:expr, $ex:expr, $r:expr) => {
            c.insert($id, mk(hwnd, hinst, $class, $text, $style, $ex, $r, $id, font, dpi));
        };
    }
    let st = w!("STATIC");
    let btn = w!("BUTTON");
    let edit = w!("EDIT");

    // 登録フォルダ
    ctl_new!(0, st, "登録フォルダ (デスクトップの何もない場所をダブルクリックで表示)", 0, 0, (10, 10, 540, 18));
    ctl_new!(IDC_LIST, w!("LISTBOX"), "", tab | WS_VSCROLL.0 | WS_BORDER.0 | LBS_NOTIFY_, 0, (10, 30, 440, 150));
    ctl_new!(IDC_UP, btn, "上へ", tab | BS_PUSHBUTTON_, 0, (462, 30, 88, 26));
    ctl_new!(IDC_DOWN, btn, "下へ", tab | BS_PUSHBUTTON_, 0, (462, 62, 88, 26));
    ctl_new!(IDC_DEL, btn, "削除", tab | BS_PUSHBUTTON_, 0, (462, 94, 88, 26));

    ctl_new!(0, st, "名前", 0, 0, (10, 196, 40, 18));
    ctl_new!(IDC_NAME, edit, "", tab | ES_AUTOHSCROLL_, edge, (52, 192, 200, 24));
    ctl_new!(0, st, "パス", 0, 0, (262, 196, 34, 18));
    ctl_new!(IDC_PATH, edit, "", tab | ES_AUTOHSCROLL_, edge, (298, 192, 180, 24));
    ctl_new!(IDC_BROWSE, btn, "参照...", tab | BS_PUSHBUTTON_, 0, (484, 191, 66, 26));
    ctl_new!(IDC_ADD, btn, "一覧に追加", tab | BS_PUSHBUTTON_, 0, (10, 226, 110, 26));
    ctl_new!(IDC_UPDATE, btn, "選択項目を更新", tab | BS_PUSHBUTTON_, 0, (126, 226, 130, 26));

    // 共通設定
    ctl_new!(0, st, "共通設定", 0, 0, (10, 268, 200, 18));
    let rows: [(i32, &str); 5] = [
        (IDC_DBL, "ダブルクリック間隔 ms (0=OS設定)"),
        (IDC_CLIPMAX, "クリップボード履歴 最大件数"),
        (IDC_GROUP, "クリップボード 直接表示件数"),
        (IDC_HISTMAX, "フォルダ履歴 最大件数 (0=無効)"),
        (IDC_MAXENT, "サブメニュー 最大項目数"),
    ];
    for (i, (id, label)) in rows.iter().enumerate() {
        let y = 292 + i as i32 * 28;
        ctl_new!(0, st, label, 0, 0, (10, y + 4, 250, 18));
        ctl_new!(*id, edit, "", tab | ES_AUTOHSCROLL_, edge, (264, y, 70, 24));
    }
    ctl_new!(IDC_HIDDEN, btn, "隠しフォルダを表示", tab | BS_AUTOCHECKBOX_, 0, (370, 292, 180, 22));
    ctl_new!(IDC_ICONS, btn, "フォルダアイコンを表示", tab | BS_AUTOCHECKBOX_, 0, (370, 320, 180, 22));
    ctl_new!(IDC_AUTOSTART, btn, "Windows 起動時に自動起動", tab | BS_AUTOCHECKBOX_, 0, (370, 348, 180, 22));

    ctl_new!(IDC_SAVE, btn, "保存", tab | BS_PUSHBUTTON_, 0, (370, 430, 84, 28));
    ctl_new!(IDC_CANCEL_BTN, btn, "キャンセル", tab | BS_PUSHBUTTON_, 0, (462, 430, 88, 28));

    // 現在の設定を反映
    let cfg = with_state(|s| s.cfg.clone()).unwrap_or_default();
    set_text(c[&IDC_DBL], &cfg.general.double_click_ms.to_string());
    set_text(c[&IDC_CLIPMAX], &cfg.clipboard.max_items.to_string());
    set_text(c[&IDC_GROUP], &cfg.clipboard.group_size.to_string());
    set_text(c[&IDC_HISTMAX], &cfg.folder_menu.history_max.to_string());
    set_text(c[&IDC_MAXENT], &cfg.folder_menu.max_entries.to_string());
    set_check(c[&IDC_HIDDEN], cfg.folder_menu.show_hidden);
    set_check(c[&IDC_ICONS], cfg.folder_menu.show_icons);
    set_check(c[&IDC_AUTOSTART], autostart_enabled());

    DLG.with(|d| *d.borrow_mut() = Some(Dlg { hwnd, font, folders: cfg.folders.clone(), ctl: c }));
    refresh_list(if cfg.folders.is_empty() { -1 } else { 0 });
    load_selected();

    let _ = ShowWindow(hwnd, SW_SHOW);
    force_foreground(hwnd);
}
