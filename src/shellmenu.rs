//! フォルダメニューの項目を右クリックしたとき、Explorer と同じコンテキストメニューを出す。
//!
//! Windows は、メニューが開いている間は同じスレッドで別の TrackPopupMenu を出せない
//! (ERROR_POPUP_ALREADY_ACTIVE)。そのため次の流れにしている:
//!   1. マウスフックが「メニュー項目の上の右クリック」を検知 (メニュー本体には渡さない)
//!   2. フォルダメニューを EndMenu() で閉じる
//!   3. 閉じた後に、IShellItem → IContextMenu で Explorer のメニューを表示・実行
//!   4. コマンドを選ばずに閉じたら、フォルダメニューを開き直す (main.rs の show_folder_popup)
//!
//! 注意: Explorer のメニューには他社のシェル拡張が読み込まれる。拡張側の不具合の影響を受ける場合がある。
//! この機能が不要なら、main.rs の `shellmenu::` を呼んでいる箇所を外せばよい。

use std::sync::atomic::AtomicBool;

use windows::core::Interface;
use windows::core::PCSTR;
use windows::Win32::System::Com::IBindCtx;

use super::*;

/// Explorer のメニュー表示中 (この間は、フォルダメニュー用のマウスフック処理を止める)
pub static SHELL_MENU_ACTIVE: AtomicBool = AtomicBool::new(false);

thread_local! {
    // 「送る」などのオーナードロー項目のため、メニュー関連メッセージを転送する先
    static SHELL_CM: RefCell<Option<(IContextMenu2, Option<IContextMenu3>)>> = const { RefCell::new(None) };
}

/// メニュー項目 (hmenu の pos 番目) が指すフォルダのパス
unsafe fn path_for_menu_item(hmenu: HMENU, pos: u32, popup_only: bool) -> Option<String> {
    let sub = GetSubMenu(hmenu, pos as i32);
    if !sub.0.is_null() {
        // サブメニュー付き項目 (展開前・展開後どちらも paths に登録済み)
        return MENU_TREE.with(|t| t.borrow().paths.get(&(sub.0 as isize)).cloned());
    }
    if popup_only {
        return None;
    }
    let id = GetMenuItemID(hmenu, pos as i32);
    if id == u32::MAX {
        return None;
    }
    lookup_target(id)
}

/// 画面座標 pt にあるフォルダ項目のパス (メニュー外・フォルダ以外の項目なら None)。
/// popup_only = true なら、サブメニュー付きの項目だけを対象にする。
/// マウスフックから呼ばれるため、軽い処理だけを行う。
pub unsafe fn path_at_point(hwnd: HWND, pt: POINT, popup_only: bool) -> Option<String> {
    let hm = HOT_MENU.load(Relaxed); // 直近の WM_MENUSELECT で選択中の項目があるメニュー
    if hm == 0 {
        return None;
    }
    let hmenu = HMENU(hm as *mut c_void);
    let n = GetMenuItemCount(hmenu);
    for pos in 0..n.max(0) {
        let mut rc = RECT::default();
        if GetMenuItemRect(hwnd, hmenu, pos as u32, &mut rc).is_ok()
            && pt.x >= rc.left
            && pt.x < rc.right
            && pt.y >= rc.top
            && pt.y < rc.bottom
        {
            return path_for_menu_item(hmenu, pos as u32, popup_only);
        }
    }
    None
}

/// Explorer のコンテキストメニューを表示する。コマンドを実行したら true、キャンセルなら false。
pub unsafe fn show_shell_menu(hwnd: HWND, path: &str, x: i32, y: i32) -> bool {
    let wp = wide(path);
    let item: IShellItem = match SHCreateItemFromParsingName(PCWSTR(wp.as_ptr()), None::<&IBindCtx>) {
        Ok(i) => i,
        Err(e) => {
            log_line(&format!("SHCreateItemFromParsingName 失敗: {path}: {e}"));
            return false;
        }
    };
    let cm: IContextMenu = match item.BindToHandler(None::<&IBindCtx>, &BHID_SFUIObject) {
        Ok(c) => c,
        Err(e) => {
            log_line(&format!("BindToHandler(IContextMenu) 失敗: {path}: {e}"));
            return false;
        }
    };
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return false,
    };
    // Shift を押しながら右クリックすると拡張メニュー (Explorer の Shift+右クリック相当)
    let flags: u32 = if GetKeyState(VK_SHIFT.0 as i32) < 0 { 0x0100 } else { 0 }; // CMF_EXTENDEDVERBS
    if let Err(e) = cm.QueryContextMenu(menu, 0, 1, 0x7FFF, flags) {
        log_line(&format!("QueryContextMenu 失敗: {path}: {e}"));
        let _ = DestroyMenu(menu);
        return false;
    }

    let cm2: Option<IContextMenu2> = cm.cast().ok();
    let cm3: Option<IContextMenu3> = cm.cast().ok();
    SHELL_CM.with(|c| *c.borrow_mut() = cm2.map(|c2| (c2, cm3)));
    SHELL_MENU_ACTIVE.store(true, Relaxed);

    force_foreground(hwnd);
    let r = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_LEFTALIGN | TPM_TOPALIGN,
        x,
        y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    SHELL_MENU_ACTIVE.store(false, Relaxed);
    SHELL_CM.with(|c| *c.borrow_mut() = None);

    let mut executed = false;
    if r.0 > 0 {
        let cmd = (r.0 - 1) as usize; // QueryContextMenu の idCmdFirst = 1 を引く
        let info = CMINVOKECOMMANDINFO {
            cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
            hwnd,
            lpVerb: PCSTR(cmd as *const u8), // MAKEINTRESOURCE 形式 (コマンド番号)
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        match cm.InvokeCommand(&info) {
            Ok(()) => executed = true,
            Err(e) => log_line(&format!("InvokeCommand 失敗: {path}: {e}")),
        }
    }
    let _ = DestroyMenu(menu);
    executed
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
