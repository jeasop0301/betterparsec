//! parsec-vdd-keepalive — 상시 Parsec 가상 디스플레이 유지 데몬 (의존성 0).
//!
//! 이미 설치된 Parsec Virtual Display Driver(VDD)를 직접 제어한다. VDD 는
//! <1초 주기로 ping(UPDATE) 해야 추가된 디스플레이가 유지되므로, 이 프로그램이
//! 상주하며 ping 한다. 핸들이 무효화(절전복귀 등)되면 자동으로 재오픈 후 재부착.
//!
//! 프로토콜 근거: github.com/nomi-san/parsec-vdd (core/parsec-vdd.h) [MIT]
//!
//! 사용:
//!   parsec-vdd-keepalive              # 상주(무한 keepalive)
//!   parsec-vdd-keepalive --test [초]  # 추가->유지->제거 (기본 12초, 검증용)
//!   parsec-vdd-keepalive --remove-all # 남은 가상 디스플레이 전부 제거하고 종료

use std::ffi::c_void;
use std::io::Write;
use std::mem;
use std::ptr;
use std::thread::sleep;
use std::time::{Duration, Instant};

type Handle = *mut c_void;
type Bool = i32;
type Dword = u32;

#[repr(C)]
struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

const VDD_ADAPTER_GUID: Guid = Guid {
    d1: 0x00b4_1627,
    d2: 0x04c4,
    d3: 0x429e,
    d4: [0xa2, 0x6e, 0x02, 0x65, 0xcf, 0x50, 0xc8, 0xfa],
};

const DIGCF_PRESENT: Dword = 0x2;
const DIGCF_DEVICEINTERFACE: Dword = 0x10;

const GENERIC_READ: Dword = 0x8000_0000;
const GENERIC_WRITE: Dword = 0x4000_0000;
const FILE_SHARE_READ: Dword = 0x1;
const FILE_SHARE_WRITE: Dword = 0x2;
const OPEN_EXISTING: Dword = 3;
const FILE_ATTRIBUTE_NORMAL: Dword = 0x80;
const FILE_FLAG_NO_BUFFERING: Dword = 0x2000_0000;
const FILE_FLAG_OVERLAPPED: Dword = 0x4000_0000;
const FILE_FLAG_WRITE_THROUGH: Dword = 0x8000_0000;

const IOCTL_ADD: Dword = 0x0022_e004;
const IOCTL_REMOVE: Dword = 0x0022_a008;
const IOCTL_UPDATE: Dword = 0x0022_a00c;
const IOCTL_VERSION: Dword = 0x0022_e010;

#[repr(C)]
struct DevInterfaceData {
    cb_size: Dword,
    interface_class_guid: Guid,
    flags: Dword,
    reserved: usize,
}

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: Handle,
}

#[repr(C)]
struct SystemTime {
    year: u16,
    month: u16,
    dow: u16,
    day: u16,
    hour: u16,
    min: u16,
    sec: u16,
    ms: u16,
}

#[link(name = "setupapi")]
extern "system" {
    fn SetupDiGetClassDevsW(class: *const Guid, en: *const u16, hwnd: Handle, flags: Dword) -> Handle;
    fn SetupDiEnumDeviceInterfaces(
        set: Handle,
        devinfo: *const c_void,
        class: *const Guid,
        index: Dword,
        out: *mut DevInterfaceData,
    ) -> Bool;
    fn SetupDiGetDeviceInterfaceDetailW(
        set: Handle,
        ifd: *const DevInterfaceData,
        detail: *mut c_void,
        detail_size: Dword,
        required: *mut Dword,
        devinfo: *mut c_void,
    ) -> Bool;
    fn SetupDiDestroyDeviceInfoList(set: Handle) -> Bool;
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: Dword,
        share: Dword,
        sec: *const c_void,
        disp: Dword,
        flags: Dword,
        tmpl: Handle,
    ) -> Handle;
    fn CreateEventW(attrs: *const c_void, manual: Bool, initial: Bool, name: *const u16) -> Handle;
    fn DeviceIoControl(
        h: Handle,
        code: Dword,
        in_buf: *const c_void,
        in_size: Dword,
        out_buf: *mut c_void,
        out_size: Dword,
        bytes_ret: *mut Dword,
        ov: *mut Overlapped,
    ) -> Bool;
    fn GetOverlappedResultEx(h: Handle, ov: *const Overlapped, transferred: *mut Dword, millis: Dword, alertable: Bool) -> Bool;
    fn CancelIoEx(h: Handle, ov: *const Overlapped) -> Bool;
    fn GetOverlappedResult(h: Handle, ov: *const Overlapped, transferred: *mut Dword, wait: Bool) -> Bool;
    fn CloseHandle(h: Handle) -> Bool;
    fn GetLocalTime(st: *mut SystemTime);
    fn GetModuleHandleW(name: *const u16) -> Handle;
}

#[link(name = "user32")]
extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, num_path: *mut u32, num_mode: *mut u32) -> i32;
    fn RegisterClassW(cls: *const WndClassW) -> u16;
    fn CreateWindowExW(ex: u32, class: *const u16, name: *const u16, style: u32, x: i32, y: i32, w: i32, h: i32, parent: Handle, menu: Handle, inst: Handle, param: *mut c_void) -> Handle;
    fn DefWindowProcW(hwnd: Handle, msg: u32, wp: usize, lp: isize) -> isize;
    fn PeekMessageW(msg: *mut Msg, hwnd: Handle, min: u32, max: u32, remove: u32) -> Bool;
    fn TranslateMessage(msg: *const Msg) -> Bool;
    fn DispatchMessageW(msg: *const Msg) -> isize;
}

#[repr(C)]
struct WndClassW {
    style: u32,
    wndproc: usize,
    cls_extra: i32,
    wnd_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu_name: *const u16,
    class_name: *const u16,
}

#[repr(C)]
struct Msg {
    hwnd: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 브로드캐스트(WM_DISPLAYCHANGE 등)를 받을 숨김 최상위 창을 만든다.
/// 순수 콘솔 프로세스는 메시지 큐가 없어 디스플레이 arrival 이 지연/누락될 수 있는데,
/// GUI 앱(ParsecDisplay/WPF)처럼 창+펌프를 두면 attach 가 제때 확정된다.
unsafe fn create_pump_window() -> Handle {
    let inst = GetModuleHandleW(ptr::null());
    let cls = wide("ParsecVddKeepAliveWnd");
    let wc = WndClassW {
        style: 0,
        wndproc: DefWindowProcW as *const () as usize,
        cls_extra: 0,
        wnd_extra: 0,
        instance: inst,
        icon: ptr::null_mut(),
        cursor: ptr::null_mut(),
        background: ptr::null_mut(),
        menu_name: ptr::null(),
        class_name: cls.as_ptr(),
    };
    RegisterClassW(&wc);
    // WS_OVERLAPPED, 비표시(숨김) — 브로드캐스트 수신용.
    CreateWindowExW(0, cls.as_ptr(), cls.as_ptr(), 0, 0, 0, 0, 0, ptr::null_mut(), ptr::null_mut(), inst, ptr::null_mut())
}

/// 대기 중인 창 메시지를 모두 처리(펌프).
unsafe fn pump() {
    let mut msg: Msg = mem::zeroed();
    while PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, 1 /*PM_REMOVE*/) != 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

fn valid(h: Handle) -> bool {
    !h.is_null() && h as isize != -1
}

fn active_displays() -> i32 {
    let (mut p, mut m) = (0u32, 0u32);
    unsafe {
        GetDisplayConfigBufferSizes(2, &mut p, &mut m);
    }
    p as i32
}

fn now() -> String {
    let mut st: SystemTime = unsafe { mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.year, st.month, st.day, st.hour, st.min, st.sec, st.ms
    )
}

fn log(msg: &str) {
    let line = format!("{}  {}", now(), msg);
    println!("{line}");
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        let d = format!("{dir}\\betterparsec");
        let _ = std::fs::create_dir_all(&d);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{d}\\parsec-vdd-keepalive.log"))
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// VDD 디바이스 핸들을 연다. 실패 시 무효 핸들 반환.
unsafe fn open_device() -> Handle {
    let set = SetupDiGetClassDevsW(
        &VDD_ADAPTER_GUID,
        ptr::null(),
        ptr::null_mut(),
        DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
    );
    if !valid(set) {
        return ptr::null_mut();
    }
    let mut handle: Handle = ptr::null_mut();
    let mut ifd: DevInterfaceData = mem::zeroed();
    ifd.cb_size = mem::size_of::<DevInterfaceData>() as Dword;
    let mut i: Dword = 0;
    while SetupDiEnumDeviceInterfaces(set, ptr::null(), &VDD_ADAPTER_GUID, i, &mut ifd) != 0 {
        let mut required: Dword = 0;
        SetupDiGetDeviceInterfaceDetailW(set, &ifd, ptr::null_mut(), 0, &mut required, ptr::null_mut());
        if required > 0 {
            let mut buf = vec![0u8; required as usize];
            // SP_DEVICE_INTERFACE_DETAIL_DATA_W.cbSize: x64=8, x86=6. 경로는 오프셋 4.
            let cb: Dword = if mem::size_of::<usize>() == 8 { 8 } else { 6 };
            (buf.as_mut_ptr() as *mut Dword).write(cb);
            let mut req2 = required;
            if SetupDiGetDeviceInterfaceDetailW(set, &ifd, buf.as_mut_ptr() as *mut c_void, required, &mut req2, ptr::null_mut()) != 0 {
                let path = buf.as_ptr().add(4) as *const u16;
                let h = CreateFileW(
                    path,
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_WRITE_THROUGH,
                    ptr::null_mut(),
                );
                if valid(h) {
                    handle = h;
                    break;
                }
            }
        }
        i += 1;
    }
    SetupDiDestroyDeviceInfoList(set);
    handle
}

/// 오버랩드 IOCTL. 실패 시 -1, 성공 시 out DWORD.
unsafe fn ioctl(vdd: Handle, code: Dword, data: &[u8], timeout: Dword) -> i32 {
    if !valid(vdd) {
        return -1;
    }
    let mut in_buf = [0u8; 32];
    let n = data.len().min(32);
    in_buf[..n].copy_from_slice(&data[..n]);
    let mut out_buf: u32 = 0;
    let mut ov: Overlapped = mem::zeroed();
    ov.h_event = CreateEventW(ptr::null(), 0, 0, ptr::null());
    DeviceIoControl(
        vdd,
        code,
        in_buf.as_ptr() as *const c_void,
        32,
        &mut out_buf as *mut u32 as *mut c_void,
        4,
        ptr::null_mut(),
        &mut ov,
    );
    let mut transferred: Dword = 0;
    let ok = GetOverlappedResultEx(vdd, &ov, &mut transferred, timeout, 0);
    if ok == 0 {
        // 대기 실패(타임아웃/에러) 시 커널에 IO가 아직 걸려 있을 수 있음 →
        // 취소하고 완료까지 블록(스택 버퍼가 나중에 덮여 다음 호출을 깨는 것 방지).
        CancelIoEx(vdd, &ov);
        let mut t2: Dword = 0;
        GetOverlappedResult(vdd, &ov, &mut t2, 1);
    }
    if !ov.h_event.is_null() {
        CloseHandle(ov.h_event);
    }
    if ok == 0 {
        return -1;
    }
    out_buf as i32
}

unsafe fn vdd_update(vdd: Handle) -> i32 {
    ioctl(vdd, IOCTL_UPDATE, &[], 1000)
}
unsafe fn vdd_add(vdd: Handle) -> i32 {
    let idx = ioctl(vdd, IOCTL_ADD, &[], 5000);
    vdd_update(vdd);
    idx
}
unsafe fn vdd_remove(vdd: Handle, index: i32) {
    // 16-bit BE index
    let d = [((index >> 8) & 0xff) as u8, (index & 0xff) as u8];
    ioctl(vdd, IOCTL_REMOVE, &d, 1000);
    vdd_update(vdd);
}
unsafe fn vdd_version(vdd: Handle) -> i32 {
    ioctl(vdd, IOCTL_VERSION, &[], 1000)
}

fn remove_all() -> i32 {
    unsafe {
        let h = open_device();
        if !valid(h) {
            log("VDD open 실패 (드라이버 상태/권한 확인)");
            return 1;
        }
        log(&format!("RemoveAll before: activeDisplays={}", active_displays()));
        for i in 0..16 {
            vdd_remove(h, i);
        }
        sleep(Duration::from_millis(600));
        log(&format!("RemoveAll after : activeDisplays={}", active_displays()));
        CloseHandle(h);
        0
    }
}

/// 데몬/테스트 공통 루프. test_secs 가 Some 이면 그 시간 후 제거하고 종료.
fn run(test_secs: Option<u64>, ping_ms: u64) -> i32 {
    let mut vdd: Handle = ptr::null_mut();
    let mut idx: i32 = -1;
    let mut fails = 0u32;
    let _hwnd = unsafe { create_pump_window() };

    let ensure = |vdd: &mut Handle, idx: &mut i32| -> bool {
        unsafe {
            if !valid(*vdd) {
                *vdd = open_device();
                if !valid(*vdd) {
                    log("VDD open 실패 (드라이버 상태/권한 확인)");
                    return false;
                }
                log(&format!("VDD open OK (driver minor={})", vdd_version(*vdd)));
                *idx = -1;
            }
            if *idx < 0 {
                let i = vdd_add(*vdd);
                if i < 0 {
                    log("가상 디스플레이 추가 실패");
                    CloseHandle(*vdd);
                    *vdd = ptr::null_mut();
                    return false;
                }
                *idx = i;
                sleep(Duration::from_millis(400));
                log(&format!("가상 디스플레이 추가됨 index={i} | activeDisplays={}", active_displays()));
            }
            true
        }
    };

    log(&format!(
        "=== parsec-vdd-keepalive 시작 (test={:?} ping={}ms) === before: activeDisplays={}",
        test_secs,
        ping_ms,
        active_displays()
    ));

    if !ensure(&mut vdd, &mut idx) {
        // 초기 실패 시 잠깐 뒤 한 번 더 (데몬이면 아래 루프에서도 재시도)
        if test_secs.is_some() {
            return 1;
        }
    }

    // add 직후 디스플레이 arrival 브로드캐스트를 확실히 펌프(약 1초).
    for _ in 0..12 {
        unsafe { pump() };
        sleep(Duration::from_millis(80));
    }

    let deadline = test_secs.map(|s| Instant::now() + Duration::from_secs(s));
    let mut last_beat = Instant::now();
    loop {
        unsafe { pump() };
        if let Some(dl) = deadline {
            if Instant::now() >= dl {
                break;
            }
        }
        if !valid(vdd) || idx < 0 {
            ensure(&mut vdd, &mut idx);
            sleep(Duration::from_millis(ping_ms));
            continue;
        }
        let r = unsafe { vdd_update(vdd) };
        if r < 0 {
            fails += 1;
            if fails >= 3 {
                log("ping 연속 실패 — 핸들 재오픈 후 재부착(절전복귀 등)");
                unsafe { CloseHandle(vdd) };
                vdd = ptr::null_mut();
                idx = -1;
                fails = 0;
                ensure(&mut vdd, &mut idx);
            }
        } else {
            fails = 0;
        }
        if test_secs.is_some() && last_beat.elapsed() >= Duration::from_secs(2) {
            log(&format!("hold: activeDisplays={}", active_displays()));
            last_beat = Instant::now();
        }
        sleep(Duration::from_millis(ping_ms));
    }

    if valid(vdd) && idx >= 0 {
        unsafe {
            vdd_remove(vdd, idx);
            sleep(Duration::from_millis(300));
            log(&format!("가상 디스플레이 제거 index={idx} | activeDisplays={}", active_displays()));
            CloseHandle(vdd);
        }
    }
    log("=== 종료 ===");
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("");
    let code = match mode {
        "--remove-all" => remove_all(),
        "--test" => {
            let secs = args.get(2).and_then(|s| s.parse::<u64>().ok()).unwrap_or(12);
            run(Some(secs), 100)
        }
        _ => run(None, 100),
    };
    std::process::exit(code);
}
