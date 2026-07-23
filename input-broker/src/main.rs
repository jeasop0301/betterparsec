mod protocol;

#[cfg(not(windows))]
fn main() {
    eprintln!("input-broker is Windows-only");
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::io;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use crate::protocol::{self, Batch, CaptureState, MessageKind};

    const INVALID_HANDLE_VALUE: isize = -1;
    const GENERIC_READ_WRITE: u32 = 0xc000_0000;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
    const PIPE_ACCESS_DUPLEX: u32 = 0x3;
    const PIPE_NOWAIT: u32 = 1;
    const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
    const ERROR_PIPE_CONNECTED: u32 = 535;
    const ERROR_PIPE_LISTENING: u32 = 536;
    const TOKEN_QUERY: u32 = 0x8;
    const TOKEN_USER: u32 = 1;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    const IOCTL_ARM: u32 = 0x0022_2000;
    const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
    const IOCTL_DISARM: u32 = 0x0022_2004;
    const IOCTL_READ_EVENTS: u32 = 0x0022_2008;
    #[allow(dead_code)]
    const IOCTL_STATUS: u32 = 0x0022_200c;
    const LEASE_MS: u32 = 500;
    const REFRESH_EVERY: Duration = Duration::from_millis(250);
    const PIPE_NAME: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'.' as u16,
        b'\\' as u16,
        b'p' as u16,
        b'i' as u16,
        b'p' as u16,
        b'e' as u16,
        b'\\' as u16,
        b'B' as u16,
        b'e' as u16,
        b't' as u16,
        b't' as u16,
        b'e' as u16,
        b'r' as u16,
        b'P' as u16,
        b'a' as u16,
        b'r' as u16,
        b's' as u16,
        b'e' as u16,
        b'c' as u16,
        b'\\' as u16,
        b'i' as u16,
        b'n' as u16,
        b'p' as u16,
        b'u' as u16,
        b't' as u16,
        b'-' as u16,
        b'v' as u16,
        b'1' as u16,
        0,
    ];
    const DEVICE_NAME: &[u16] = &[
        b'\\' as u16,
        b'\\' as u16,
        b'.' as u16,
        b'\\' as u16,
        b'B' as u16,
        b'e' as u16,
        b't' as u16,
        b't' as u16,
        b'e' as u16,
        b'r' as u16,
        b'P' as u16,
        b'a' as u16,
        b'r' as u16,
        b's' as u16,
        b'e' as u16,
        b'c' as u16,
        b'K' as u16,
        b'b' as u16,
        b'd' as u16,
        0,
    ];

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: i32,
    }
    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: u32,
    }
    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }
    #[repr(C)]
    struct ServiceTableEntry {
        service_name: *mut u16,
        service_proc: Option<unsafe extern "system" fn(u32, *mut *mut u16)>,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *const SecurityAttributes,
            disposition: u32,
            flags: u32,
            template: isize,
        ) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn DeviceIoControl(
            device: isize,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer: u32,
            in_buffer: u32,
            timeout: u32,
            security: *const SecurityAttributes,
        ) -> isize;
        fn ConnectNamedPipe(pipe: isize, overlapped: *mut c_void) -> i32;
        fn DisconnectNamedPipe(pipe: isize) -> i32;
        fn ReadFile(
            handle: isize,
            buffer: *mut c_void,
            bytes: u32,
            read: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn WriteFile(
            handle: isize,
            buffer: *const c_void,
            bytes: u32,
            written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn PeekNamedPipe(
            pipe: isize,
            buffer: *mut c_void,
            bytes: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
        fn GetNamedPipeClientSessionId(pipe: isize, client_session_id: *mut u32) -> i32;
        fn GetCurrentProcess() -> isize;
        fn LocalFree(memory: isize) -> isize;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
        fn GetTokenInformation(
            token: isize,
            class: u32,
            info: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> i32;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl: *const u16,
            revision: u32,
            descriptor: *mut *mut c_void,
            size: *mut u32,
        ) -> i32;
    }
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptGenRandom(algorithm: isize, buffer: *mut u8, buffer_len: u32, flags: u32) -> i32;
    }
    #[link(name = "wtsapi32")]
    unsafe extern "system" {
        fn WTSQueryUserToken(session: u32, token: *mut isize) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WTSGetActiveConsoleSessionId() -> u32;
    }
    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn StartServiceCtrlDispatcherW(table: *const ServiceTableEntry) -> i32;
        fn RegisterServiceCtrlHandlerW(
            name: *const u16,
            handler: Option<unsafe extern "system" fn(u32)>,
        ) -> isize;
        fn SetServiceStatus(handle: isize, status: *const ServiceStatus) -> i32;
    }
    #[repr(C)]
    struct ServiceStatus {
        service_type: u32,
        current_state: u32,
        controls_accepted: u32,
        win32_exit_code: u32,
        service_specific_exit_code: u32,
        check_point: u32,
        wait_hint: u32,
    }

    static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
    static SERVICE_NAME: &[u16] = &[
        b'B' as u16,
        b'e' as u16,
        b't' as u16,
        b't' as u16,
        b'e' as u16,
        b'r' as u16,
        b'P' as u16,
        b'a' as u16,
        b'r' as u16,
        b's' as u16,
        b'e' as u16,
        b'c' as u16,
        b'I' as u16,
        b'n' as u16,
        b'p' as u16,
        b'u' as u16,
        b't' as u16,
        0,
    ];

    pub fn main() {
        let service = std::env::args().skip(1).any(|arg| arg == "--service");
        let console = std::env::args().skip(1).any(|arg| arg == "--console");
        if service == console {
            eprintln!("use exactly one of --console or --service");
            std::process::exit(2);
        }
        let result = if service {
            run_service()
        } else {
            run_console()
        };
        if result.is_err() {
            // Do not expose input contents or protocol payloads in diagnostics.
            eprintln!("input broker stopped due to an error");
            std::process::exit(1);
        }
    }

    fn run_console() -> io::Result<()> {
        run_broker(false)
    }

    fn run_service() -> io::Result<()> {
        let table = [
            ServiceTableEntry {
                service_name: SERVICE_NAME.as_ptr() as *mut u16,
                service_proc: Some(service_main),
            },
            ServiceTableEntry {
                service_name: null_mut(),
                service_proc: None,
            },
        ];
        if unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    unsafe extern "system" fn service_main(_: u32, _: *mut *mut u16) {
        let status_handle =
            unsafe { RegisterServiceCtrlHandlerW(SERVICE_NAME.as_ptr(), Some(service_control)) };
        if status_handle == 0 {
            return;
        }
        let running = ServiceStatus {
            service_type: 0x10,
            current_state: 4,
            controls_accepted: 1,
            win32_exit_code: 0,
            service_specific_exit_code: 0,
            check_point: 0,
            wait_hint: 0,
        };
        unsafe {
            SetServiceStatus(status_handle, &running);
        }
        let result = run_broker(true);
        let stopped = ServiceStatus {
            service_type: 0x10,
            current_state: 1,
            controls_accepted: 0,
            win32_exit_code: if result.is_ok() { 0 } else { 1 },
            service_specific_exit_code: 0,
            check_point: 0,
            wait_hint: 0,
        };
        unsafe {
            SetServiceStatus(status_handle, &stopped);
        }
    }

    unsafe extern "system" fn service_control(control: u32) {
        if control == 1 || control == 5 {
            STOP_REQUESTED.store(true, Ordering::Release);
        }
    }

    enum ClientWait {
        Connected,
        SessionChanged,
        Stop,
    }

    fn session_is_current(expected: Option<u32>) -> bool {
        expected.is_none_or(|session| unsafe { WTSGetActiveConsoleSessionId() } == session)
    }

    fn verify_client_session(pipe: isize, expected: Option<u32>) -> io::Result<()> {
        let Some(expected) = expected else {
            return Ok(());
        };
        let mut actual = u32::MAX;
        if unsafe { GetNamedPipeClientSessionId(pipe, &mut actual) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if actual != expected || !session_is_current(Some(expected)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe client is not in the active console session",
            ));
        }
        Ok(())
    }

    fn run_broker(as_service: bool) -> io::Result<()> {
        STOP_REQUESTED.store(false, Ordering::Release);
        let mut device = loop {
            match Device::open() {
                Ok(device) => break device,
                Err(_) if as_service && !STOP_REQUESTED.load(Ordering::Acquire) => {
                    std::thread::sleep(Duration::from_secs(1));
                }
                Err(error) => return Err(error),
            }
        };
        while !STOP_REQUESTED.load(Ordering::Acquire) {
            let security = match PipeSecurity::for_active_user(as_service) {
                Ok(security) => security,
                Err(_) if as_service && !STOP_REQUESTED.load(Ordering::Acquire) => {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let pipe = create_pipe(&security)?;
            match wait_for_client(pipe.0, security.session)? {
                ClientWait::Connected => {}
                ClientWait::SessionChanged => continue,
                ClientWait::Stop => break,
            }
            if verify_client_session(pipe.0, security.session).is_err() {
                unsafe {
                    DisconnectNamedPipe(pipe.0);
                }
                let _ = device.disarm();
                continue;
            }
            let result = serve_client(&mut device, pipe.0, security.session);
            unsafe {
                DisconnectNamedPipe(pipe.0);
            }
            let _ = device.disarm();
            if result.is_err() {
                continue;
            }
        }
        let _ = device.disarm();
        Ok(())
    }

    fn wait_for_client(pipe: isize, expected_session: Option<u32>) -> io::Result<ClientWait> {
        while !STOP_REQUESTED.load(Ordering::Acquire) {
            if !session_is_current(expected_session) {
                return Ok(ClientWait::SessionChanged);
            }
            if unsafe { ConnectNamedPipe(pipe, null_mut()) } != 0 {
                return Ok(ClientWait::Connected);
            }
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED => return Ok(ClientWait::Connected),
                ERROR_PIPE_LISTENING => std::thread::sleep(Duration::from_millis(25)),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Ok(ClientWait::Stop)
    }

    fn serve_client(
        device: &mut Device,
        pipe: isize,
        expected_session: Option<u32>,
    ) -> io::Result<()> {
        let client = 1;
        let mut state = CaptureState::default();
        state.connect(client).expect("new pipe has one client");
        let mut last_refresh = Instant::now() - REFRESH_EVERY;
        while !STOP_REQUESTED.load(Ordering::Acquire) {
            if !session_is_current(expected_session) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "active console session changed",
                ));
            }
            if pipe_available(pipe)? {
                let header = read_header(pipe)?;
                let payload = read_exact(pipe, header.payload_len as usize)?;
                match header.kind {
                    MessageKind::Arm if payload.is_empty() => {
                        let nonce = device.begin_capture()?;
                        if let Err(error) = state.arm(client, nonce) {
                            let _ = device.disarm();
                            return Err(protocol_error(error));
                        }
                        last_refresh = Instant::now();
                        write_message(pipe, MessageKind::Status, &[1])?;
                    }
                    MessageKind::Disarm if payload.is_empty() => {
                        state.disarm(client).map_err(protocol_error)?;
                        device.disarm()?;
                        write_message(pipe, MessageKind::Status, &[0])?;
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid control message",
                        ));
                    }
                }
            }
            if state.is_armed() {
                if last_refresh.elapsed() >= REFRESH_EVERY {
                    if device.refresh_lease().is_err() {
                        state.fail_closed();
                        let _ = device.disarm();
                        return Err(io::Error::other("lease refresh failed"));
                    }
                    last_refresh = Instant::now();
                }
                let batch = device.read_batch()?;
                if !batch.events.is_empty() || batch.dropped != 0 {
                    state
                        .observe_batch(client, &batch)
                        .map_err(protocol_error)?;
                    let payload = protocol::encode_batch(&batch).map_err(protocol_error)?;
                    write_message(pipe, MessageKind::Events, &payload)?;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        state.disconnect(client).map_err(protocol_error)?;
        Ok(())
    }

    fn protocol_error(_: protocol::ProtocolError) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, "invalid input protocol")
    }
    fn pipe_available(pipe: isize) -> io::Result<bool> {
        let mut header_bytes = [0u8; 12];
        let mut peeked = 0;
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                pipe,
                header_bytes.as_mut_ptr().cast(),
                header_bytes.len() as u32,
                &mut peeked,
                &mut available,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if peeked < header_bytes.len() as u32 {
            return Ok(false);
        }
        let header = protocol::decode_header(&header_bytes).map_err(protocol_error)?;
        Ok(available as usize >= header_bytes.len() + header.payload_len as usize)
    }
    fn read_header(pipe: isize) -> io::Result<protocol::Header> {
        protocol::decode_header(&read_exact(pipe, 12)?).map_err(protocol_error)
    }
    fn read_exact(pipe: isize, length: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        let mut offset = 0;
        while offset < length {
            let mut read = 0;
            if unsafe {
                ReadFile(
                    pipe,
                    bytes[offset..].as_mut_ptr().cast(),
                    (length - offset) as u32,
                    &mut read,
                    null_mut(),
                )
            } == 0
                || read == 0
            {
                return Err(io::Error::last_os_error());
            }
            offset += read as usize;
        }
        Ok(bytes)
    }
    fn write_message(pipe: isize, kind: MessageKind, payload: &[u8]) -> io::Result<()> {
        let header = protocol::encode_header(kind, payload.len()).map_err(protocol_error)?;
        write_all(pipe, &header)?;
        write_all(pipe, payload)
    }
    fn write_all(pipe: isize, bytes: &[u8]) -> io::Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written = 0;
            if unsafe {
                WriteFile(
                    pipe,
                    bytes[offset..].as_ptr().cast(),
                    (bytes.len() - offset) as u32,
                    &mut written,
                    null_mut(),
                )
            } == 0
                || written == 0
            {
                return Err(io::Error::last_os_error());
            }
            offset += written as usize;
        }
        Ok(())
    }

    struct Handle(isize);
    impl Drop for Handle {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE && self.0 != 0 {
                unsafe {
                    CloseHandle(self.0);
                }
            }
        }
    }
    struct Device {
        handle: Handle,
        nonce: u64,
    }
    impl Device {
        fn open() -> io::Result<Self> {
            let handle = unsafe {
                CreateFileW(
                    DEVICE_NAME.as_ptr(),
                    GENERIC_READ_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    0,
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self {
                    handle: Handle(handle),
                    nonce: 0,
                })
            }
        }
        fn begin_capture(&mut self) -> io::Result<u64> {
            let mut nonce = 0u64;
            while nonce == 0 {
                let status = unsafe {
                    BCryptGenRandom(
                        0,
                        (&mut nonce as *mut u64).cast(),
                        std::mem::size_of::<u64>() as u32,
                        BCRYPT_USE_SYSTEM_PREFERRED_RNG,
                    )
                };
                if status != 0 {
                    return Err(io::Error::other(format!(
                        "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
                        status as u32
                    )));
                }
            }
            self.nonce = nonce;
            self.refresh_lease()?;
            Ok(self.nonce)
        }
        fn refresh_lease(&mut self) -> io::Result<()> {
            let input = [
                1u8,
                0,
                0,
                0,
                (LEASE_MS & 0xff) as u8,
                (LEASE_MS >> 8) as u8,
                0,
                0,
                self.nonce as u8,
                (self.nonce >> 8) as u8,
                (self.nonce >> 16) as u8,
                (self.nonce >> 24) as u8,
                (self.nonce >> 32) as u8,
                (self.nonce >> 40) as u8,
                (self.nonce >> 48) as u8,
                (self.nonce >> 56) as u8,
            ];
            self.ioctl(IOCTL_ARM, &input, &mut [])
        }
        fn disarm(&mut self) -> io::Result<()> {
            let deadline = Instant::now() + Duration::from_millis(100);
            loop {
                match self.ioctl(IOCTL_DISARM, &[], &mut []) {
                    Ok(()) => return Ok(()),
                    Err(error)
                        if error.raw_os_error() == Some(170) && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        fn read_batch(&mut self) -> io::Result<Batch> {
            let mut output = vec![0; protocol::MAX_PAYLOAD];
            let mut returned = 0;
            if unsafe {
                DeviceIoControl(
                    self.handle.0,
                    IOCTL_READ_EVENTS,
                    null(),
                    0,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            output.truncate(returned as usize);
            protocol::decode_batch(&output).map_err(protocol_error)
        }
        fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<()> {
            let mut returned = 0;
            if unsafe {
                DeviceIoControl(
                    self.handle.0,
                    code,
                    input.as_ptr().cast(),
                    input.len() as u32,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    struct PipeSecurity {
        descriptor: *mut c_void,
        session: Option<u32>,
    }
    impl PipeSecurity {
        fn for_active_user(service: bool) -> io::Result<Self> {
            let (token, session) = if service {
                let (token, session) = active_user_token()?;
                (token, Some(session))
            } else {
                (process_token()?, None)
            };
            let token = Handle(token);
            let mut bytes = 0;
            unsafe {
                GetTokenInformation(token.0, TOKEN_USER, null_mut(), 0, &mut bytes);
            }
            if bytes == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut user = vec![0u8; bytes as usize];
            if unsafe {
                GetTokenInformation(
                    token.0,
                    TOKEN_USER,
                    user.as_mut_ptr().cast(),
                    bytes,
                    &mut bytes,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let token_user = unsafe { std::ptr::read_unaligned(user.as_ptr().cast::<TokenUser>()) };
            let sid = token_user.user.sid;
            let mut sid_string = null_mut();
            if unsafe { ConvertSidToStringSidW(sid, &mut sid_string) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let sid_text = unsafe { wide_string(sid_string) };
            unsafe {
                LocalFree(sid_string as isize);
            }
            let sddl = format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;{sid_text})");
            let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut descriptor = null_mut();
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SECURITY_DESCRIPTOR_REVISION,
                    &mut descriptor,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                descriptor,
                session,
            })
        }
    }
    impl Drop for PipeSecurity {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                unsafe {
                    LocalFree(self.descriptor as isize);
                }
            }
        }
    }
    fn process_token() -> io::Result<isize> {
        let mut token = 0;
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(token)
        }
    }
    fn active_user_token() -> io::Result<(isize, u32)> {
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        if session == u32::MAX {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no active interactive user",
            ));
        }
        let mut token = 0;
        if unsafe { WTSQueryUserToken(session, &mut token) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok((token, session))
        }
    }
    unsafe fn wide_string(pointer: *const u16) -> String {
        let mut length = 0;
        while unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) })
    }
    fn create_pipe(security: &PipeSecurity) -> io::Result<Handle> {
        let attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32,
            security_descriptor: security.descriptor,
            inherit_handle: 0,
        };
        let pipe = unsafe {
            CreateNamedPipeW(
                PIPE_NAME.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                protocol::MAX_PAYLOAD as u32,
                protocol::MAX_PAYLOAD as u32,
                0,
                &attributes,
            )
        };
        if pipe == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(pipe))
        }
    }
}

#[cfg(windows)]
fn main() {
    windows::main();
}
