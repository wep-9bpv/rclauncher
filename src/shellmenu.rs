//! フォルダメニューの項目を右クリックしたとき、Explorer と同じコンテキストメニューを出す。
//!
//! 仕組み:
//!   1. フォルダメニュー(TrackPopupMenu)表示中に WM_MENURBUTTONUP が届く
//!   2. 項目 → フォルダのパスを引く
//!   3. IShellItem → IContextMenu を取得し、入れ子の TrackPopupMenu で表示
//!   4. 選ばれたコマンドを InvokeCommand で実行し、フォルダメニューも閉じる
//!
//! 注意: Explorer のメニューには他社のシェル拡張が読み込まれる。拡張側の不具合の影響を受ける場合がある。
//! この機能が不要なら、main.rs の `shellmenu::` を呼んでいる箇所(3か所)を外せばよい。

use std::sync::atomic::AtomicBool;

use windows::core::{Interface, PCSTR};
use windows::Win32::System::Com::IBindCtx;

use super::*;

/// Explorer のメニュー表示中 (この間はフォルダメニュー側のクリック処理を止める)
pub static SHELL_MENU_ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    // 「送る」などのオーナードロー項目のため、メニュー関連メッセージを転送する先
    static SHELL_CM: RefCell<Option<(IContextMenu2, Option<IContextMenu3>)>> = const { RefCell::new(None) };
}

/// メニュー項目 (hmenu の pos 番目) が指すフォルダのパス
unsafe fn path_for_menu_item(hmenu: HMENU, pos: u32) -> Option<String> {
    let sub = GetSubMenu(hmenu, pos as i32);
    if !sub.0.is_null() {
        // サブメニュー付き項目 (展開前・展開後どちらも paths に登録済み)
        return MENU_TREE.with(|t| t.borrow().paths.get(&(sub.0 as isize)).cloned());
    }
    let id = GetMenuItemID(hmenu, pos as i32);
    if id == u32::MAX {
        return None;
    }
    lookup_target(id)
}

/// WM_MENURBUTTONUP: wparam = 項目の位置, lparam = メニューのハンドル
pub unsafe fn on_menu_rbutton(hwnd: HWND, pos: u32, hmenu: HMENU) {
    if !FOLDER_MENU_ACTIVE.load(Relaxed) || SHELL_MENU_ACTIVE.load(Relaxed) {
        return;
    }
    let Some(path) = path_for_menu_item(hmenu, pos) else {
        return;
    };
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    show_shell_menu(hwnd, &path, pt.x, pt.y);
}

unsafe fn show_shell_menu(hwnd: HWND, path: &str, x: i32, y: i32) {
    let wp = wide(path);
    let item: IShellItem = match SHCreateItemFromParsingName(PCWSTR(wp.as_ptr()), None::<&IBindCtx>) {
        Ok(i) => i,
        Err(_) => return,
    };
    let cm: IContextMenu = match item.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject) {
        Ok(c) => c,
        Err(_) => return,
    };
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    // Shift を押しながら右クリックすると拡張メニュー(Explorer の Shift+右クリック相当)
    let flags: u32 = if GetKeyState(VK_SHIFT.0 as i32) < 0 { 0x0100 } else { 0 }; // CMF_EXTENDEDVERBS
    if cm.QueryContextMenu(menu, 0, 1, 0x7FFF, flags).is_err() {
        let _ = DestroyMenu(menu);
        return;
    }

    let cm2: Option<IContextMenu2> = cm.cast().ok();
    let cm3: Option<IContextMenu3> = cm.cast().ok();
    SHELL_CM.with(|c| *c.borrow_mut() = cm2.map(|c2| (c2, cm3)));
    SHELL_MENU_ACTIVE.store(true, Relaxed);

    let r = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
        x,
        y,
        0,
        hwnd,
        None,
    );

    SHELL_MENU_ACTIVE.store(false, Relaxed);
    SHELL_CM.with(|c| *c.borrow_mut() = None);

    if r.0 > 0 {
        let cmd = (r.0 - 1) as usize; // QueryContextMenu の idCmdFirst = 1 を引く
        let info = CMINVOKECOMMANDINFO {
            cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
            hwnd,
            lpVerb: PCSTR(cmd as *const u8), // MAKEINTRESOURCE 形式 (コマンド番号)
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let _ = cm.InvokeCommand(&info);
        let _ = EndMenu(); // 外側のフォルダメニューも閉じる
    }
    let _ = DestroyMenu(menu);
}

/// Explorer のメニュー表示中に届くメッセージを、シェルのメニュー処理へ転送する。
/// 転送した場合は Some(結果) を返す (wndproc はそのまま返す)。
pub unsafe fn forward_menu_msg(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !SHELL_MENU_ACTIVE.load(Relaxed) {
        return None;
    }
    if !matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        return None;
    }
    // borrow を保持したまま呼ばない (呼び出し中に再入する可能性があるため)
    let cm = SHELL_CM.with(|c| c.borrow().clone())?;
    if let Some(cm3) = &cm.1 {
        let mut res = LRESULT(0);
        if cm3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut res)).is_ok() {
            return Some(res);
        }
    }
    let _ = cm.0.HandleMenuMsg(msg, wparam, lparam);
    Some(LRESULT(if msg == WM_INITMENUPOPUP { 0 } else { 1 }))
}
