//! Empirical WH_KEYBOARD_LL delivery probe (07-17 Alt+Tab field issue:
//! hook installs report success but the proc receives ZERO events).
//! Replicates input.rs's exact pattern — dedicated pump thread,
//! TIME_CRITICAL priority, global LL hook — then injects key events via
//! SendInput (LL hooks see injected input, LLKHF_INJECTED) and reports
//! how many the proc observed. Run on any machine where the hook
//! misbehaves: `cargo run -p app-native --bin hook_probe`.
//! Exit code 0 = events delivered, 1 = zero events (hook dead/blocked).

#[cfg(windows)]
fn main() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
        VK_SHIFT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW,
        PostThreadMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WM_QUIT, WM_USER,
    };

    static EVENTS: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn probe_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            EVENTS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    let (tx, rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let pump = std::thread::spawn(move || unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(probe_proc), None, 0) {
            Ok(hook) => {
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
                let _ = tx.send(Ok(windows::Win32::System::Threading::GetCurrentThreadId()));
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
            }
        }
    });
    let thread_id = match rx.recv() {
        Ok(Ok(id)) => {
            println!("hook installed (pump thread {id})");
            id
        }
        Ok(Err(e)) => {
            eprintln!("SetWindowsHookExW failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("pump thread died");
            std::process::exit(1);
        }
    };

    // RegisterHotKey(Alt+Tab) viability probe (07-17g field log: the
    // registration fails on every attempt on the tester's machine —
    // this tells us whether that is machine-specific or OS-reserved).
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            MOD_ALT, RegisterHotKey, UnregisterHotKey, VK_TAB,
        };
        match unsafe { RegisterHotKey(None, 42, MOD_ALT, VK_TAB.0 as u32) } {
            Ok(()) => {
                println!("RegisterHotKey(Alt+Tab): OK");
                unsafe {
                    let _ = UnregisterHotKey(None, 42);
                }
            }
            Err(e) => println!("RegisterHotKey(Alt+Tab): FAILED — {e}"),
        }
    }

    // Inject 5 SHIFT down/up pairs, spaced out so each is a distinct event.
    for _ in 0..5 {
        let mk = |flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_SHIFT,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            SendInput(
                &[mk(KEYBD_EVENT_FLAGS(0)), mk(KEYEVENTF_KEYUP)],
                size_of::<INPUT>() as i32,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    unsafe {
        let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
    }
    let _ = pump.join();

    let n = EVENTS.load(Ordering::SeqCst);
    println!("events observed by LL hook proc: {n} (expected ~10)");
    std::process::exit(if n > 0 { 0 } else { 1 });
}

#[cfg(not(windows))]
fn main() {
    eprintln!("windows-only probe");
}
