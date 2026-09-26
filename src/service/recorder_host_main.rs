//! Standalone Windows GUI host, embedded in the main application at build time.
//! It intentionally depends only on std and the Windows system libraries.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod windows {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::mem::{size_of, size_of_val};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    type Handle = *mut c_void;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const DUPLICATE_SAME_ACCESS: u32 = 2;
    const EXTENDED_STARTUPINFO_PRESENT: u32 = 0x0008_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const STARTF_USESTDHANDLES: u32 = 0x100;
    const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
    const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002_000d;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const INFINITE: u32 = u32::MAX;

    #[derive(Default)]
    #[repr(C)]
    struct StartupInfo {
        cb: u32,
        reserved: *mut u16,
        desktop: *mut u16,
        title: *mut u16,
        x: u32,
        y: u32,
        x_size: u32,
        y_size: u32,
        x_count: u32,
        y_count: u32,
        fill_attribute: u32,
        flags: u32,
        show_window: u16,
        reserved_size: u16,
        reserved_bytes: *mut u8,
        stdin: Handle,
        stdout: Handle,
        stderr: Handle,
    }
    #[derive(Default)]
    #[repr(C)]
    struct StartupInfoEx {
        startup: StartupInfo,
        attributes: *mut c_void,
    }
    #[derive(Default)]
    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }
    #[derive(Default)]
    #[repr(C)]
    struct BasicLimitInformation {
        process_time: i64,
        job_time: i64,
        flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }
    #[derive(Default)]
    #[repr(C)]
    struct ExtendedLimitInformation {
        basic: BasicLimitInformation,
        io_counters: [u64; 6],
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CloseHandle(handle: Handle) -> i32;
        fn GetCurrentProcess() -> Handle;
        fn DuplicateHandle(
            source_process: Handle,
            source: Handle,
            target_process: Handle,
            target: *mut Handle,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(
            job: Handle,
            class: u32,
            information: *const c_void,
            size: u32,
        ) -> i32;
        fn InitializeProcThreadAttributeList(
            list: *mut c_void,
            count: u32,
            flags: u32,
            size: *mut usize,
        ) -> i32;
        fn UpdateProcThreadAttribute(
            list: *mut c_void,
            flags: u32,
            attribute: usize,
            value: *mut c_void,
            size: usize,
            previous: *mut c_void,
            return_size: *mut usize,
        ) -> i32;
        fn DeleteProcThreadAttributeList(list: *mut c_void);
        fn CreateProcessW(
            application: *const u16,
            command: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit: i32,
            flags: u32,
            environment: *const c_void,
            directory: *const u16,
            startup: *const StartupInfo,
            information: *mut ProcessInformation,
        ) -> i32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, code: *mut u32) -> i32;
    }
    #[link(name = "bcrypt")]
    unsafe extern "system" {
        fn BCryptOpenAlgorithmProvider(
            provider: *mut Handle,
            algorithm: *const u16,
            implementation: *const u16,
            flags: u32,
        ) -> i32;
        fn BCryptGetProperty(
            object: Handle,
            property: *const u16,
            output: *mut u8,
            length: u32,
            result: *mut u32,
            flags: u32,
        ) -> i32;
        fn BCryptCreateHash(
            provider: Handle,
            hash: *mut Handle,
            object: *mut u8,
            object_size: u32,
            secret: *const u8,
            secret_size: u32,
            flags: u32,
        ) -> i32;
        fn BCryptHashData(hash: Handle, data: *const u8, length: u32, flags: u32) -> i32;
        fn BCryptFinishHash(hash: Handle, output: *mut u8, length: u32, flags: u32) -> i32;
        fn BCryptDestroyHash(hash: Handle) -> i32;
        fn BCryptCloseAlgorithmProvider(provider: Handle, flags: u32) -> i32;
    }

    struct OwnedHandle(Handle);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Attributes {
        storage: Vec<usize>,
    }
    impl Attributes {
        fn pointer(&mut self) -> *mut c_void {
            self.storage.as_mut_ptr().cast()
        }
    }
    impl Drop for Attributes {
        fn drop(&mut self) {
            unsafe {
                DeleteProcThreadAttributeList(self.pointer());
            }
        }
    }
    fn wide(value: &OsStr) -> io::Result<Vec<u16>> {
        let mut result: Vec<u16> = value.encode_wide().collect();
        if result.contains(&0) {
            return Err(invalid("NUL in process argument"));
        }
        result.push(0);
        Ok(result)
    }
    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }
    fn check(value: i32) -> io::Result<()> {
        if value == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    fn crypto_check(status: i32) -> io::Result<()> {
        if status < 0 {
            Err(io::Error::other(format!("BCrypt failed: {status:#x}")))
        } else {
            Ok(())
        }
    }
    fn digest(file: &mut File) -> io::Result<String> {
        struct Provider(Handle);
        impl Drop for Provider {
            fn drop(&mut self) {
                unsafe {
                    BCryptCloseAlgorithmProvider(self.0, 0);
                }
            }
        }
        struct Hash(Handle);
        impl Drop for Hash {
            fn drop(&mut self) {
                unsafe {
                    BCryptDestroyHash(self.0);
                }
            }
        }
        let mut provider = null_mut();
        unsafe {
            crypto_check(BCryptOpenAlgorithmProvider(
                &mut provider,
                wide(OsStr::new("SHA256"))?.as_ptr(),
                null(),
                0,
            ))?;
        }
        let provider = Provider(provider);
        let mut object_length = 0u32;
        let mut written = 0;
        unsafe {
            crypto_check(BCryptGetProperty(
                provider.0,
                wide(OsStr::new("ObjectLength"))?.as_ptr(),
                (&mut object_length as *mut u32).cast(),
                4,
                &mut written,
                0,
            ))?;
        }
        if written != 4 || object_length > 1024 * 1024 {
            return Err(invalid("invalid SHA256 object size"));
        }
        let mut object = vec![0u8; object_length as usize];
        let mut hash = null_mut();
        unsafe {
            crypto_check(BCryptCreateHash(
                provider.0,
                &mut hash,
                object.as_mut_ptr(),
                object_length,
                null(),
                0,
                0,
            ))?;
        }
        let hash = Hash(hash);
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            unsafe {
                crypto_check(BCryptHashData(hash.0, buffer.as_ptr(), length as u32, 0))?;
            }
        }
        let mut output = [0u8; 32];
        unsafe {
            crypto_check(BCryptFinishHash(hash.0, output.as_mut_ptr(), 32, 0))?;
        }
        Ok(output.iter().map(|byte| format!("{byte:02x}")).collect())
    }
    fn open_image(path: &Path, expected: &str) -> io::Result<File> {
        if expected.len() != 64
            || !expected
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(invalid("invalid expected image checksum"));
        }
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || metadata.len() == 0
            || metadata.len() > 128 * 1024 * 1024
        {
            return Err(invalid("image must be a bounded regular file"));
        }
        if digest(&mut file)? != expected {
            return Err(invalid("recorder host image checksum mismatch"));
        }
        // The read-only sharing handle remains open through process creation,
        // preventing modification or replacement after verification.
        Ok(file)
    }
    fn quote(argument: &OsStr) -> Vec<u16> {
        let mut result = vec![b'"' as u16];
        let mut slashes = 0;
        for value in argument.encode_wide() {
            if value == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            if value == b'"' as u16 {
                result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2 + 1));
            } else {
                result.extend(std::iter::repeat_n(b'\\' as u16, slashes));
            }
            slashes = 0;
            result.push(value);
        }
        result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        result.push(b'"' as u16);
        result
    }
    fn inherited(file: &File) -> io::Result<OwnedHandle> {
        let mut result = null_mut();
        unsafe {
            check(DuplicateHandle(
                GetCurrentProcess(),
                file.as_raw_handle(),
                GetCurrentProcess(),
                &mut result,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            ))?;
        }
        Ok(OwnedHandle(result))
    }
    pub fn run() -> io::Result<i32> {
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        if args.as_slice() == [OsString::from("--host-info")] {
            println!(
                "{{\"schemaVersion\":1,\"product\":\"codex-usage-monit-recorder-host\",\"version\":\"{}\",\"buildId\":\"{}\",\"target\":\"{}\"}}",
                env!("CARGO_PKG_VERSION"),
                env!("MONIT_BUILD_ID"),
                env!("MONIT_BUILD_TARGET")
            );
            return Ok(0);
        }
        if args.len() < 12
            || args[0] != "--host-sha256"
            || args[2] != "--recorder-executable"
            || args[4] != "--recorder-sha256"
            || args[6] != "--log-file"
            || args[8] != "--"
        {
            return Err(invalid("unsupported recorder host invocation"));
        }
        let host = std::env::current_exe()?.canonicalize()?;
        let recorder = PathBuf::from(&args[3]);
        let log_path = PathBuf::from(&args[7]);
        if !recorder.is_absolute() || !log_path.is_absolute() {
            return Err(invalid("host paths must be absolute"));
        }
        let recorder = recorder.canonicalize()?;
        if recorder.parent() != host.parent()
            || recorder.file_name() != Some(OsStr::new("codex-usage-monit.exe"))
        {
            return Err(invalid("host only runs its sibling managed recorder"));
        }
        let child_args = &args[9..];
        if !child_args
            .windows(2)
            .any(|v| v[0] == "record" && v[1] == "--foreground")
            || !child_args
                .windows(2)
                .any(|v| v[0] == "--service-cutover-protocol" && v[1] == "source-aware-v2")
        {
            return Err(invalid(
                "host requires a managed foreground recorder contract",
            ));
        }
        let _host_image = open_image(
            &host,
            args[1]
                .to_str()
                .ok_or_else(|| invalid("non-Unicode checksum"))?,
        )?;
        let _recorder_image = open_image(
            &recorder,
            args[5]
                .to_str()
                .ok_or_else(|| invalid("non-Unicode checksum"))?,
        )?;
        let log = OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&log_path)?;
        if log.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("host log must not be a reparse point"));
        }
        let stdin = inherited(&File::open("NUL")?)?;
        let log = inherited(&log)?;
        let job = unsafe { CreateJobObjectW(null(), null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = OwnedHandle(job);
        let mut limits = ExtendedLimitInformation::default();
        limits.basic.flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            check(SetInformationJobObject(
                job.0,
                9,
                (&limits as *const ExtendedLimitInformation).cast(),
                size_of::<ExtendedLimitInformation>() as u32,
            ))?;
        }
        let mut attribute_size = 0;
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut attribute_size);
        }
        if attribute_size == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut storage = vec![0usize; attribute_size.div_ceil(size_of::<usize>())];
        unsafe {
            check(InitializeProcThreadAttributeList(
                storage.as_mut_ptr().cast(),
                2,
                0,
                &mut attribute_size,
            ))?;
        }
        let mut attributes = Attributes { storage };
        let mut handles = [stdin.0, log.0];
        let mut jobs = [job.0];
        unsafe {
            check(UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
                handles.as_mut_ptr().cast(),
                size_of_val(&handles),
                null_mut(),
                null_mut(),
            ))?;
            check(UpdateProcThreadAttribute(
                attributes.pointer(),
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST,
                jobs.as_mut_ptr().cast(),
                size_of_val(&jobs),
                null_mut(),
                null_mut(),
            ))?;
        }
        let mut startup = StartupInfoEx::default();
        startup.startup.cb = size_of::<StartupInfoEx>() as u32;
        startup.startup.flags = STARTF_USESTDHANDLES;
        startup.startup.stdin = stdin.0;
        startup.startup.stdout = log.0;
        startup.startup.stderr = log.0;
        startup.attributes = attributes.pointer();
        let mut command = quote(recorder.as_os_str());
        for argument in child_args {
            command.push(b' ' as u16);
            command.extend(quote(argument));
        }
        if command.contains(&0) || command.len() >= 32_767 {
            return Err(invalid("recorder command is not representable"));
        }
        command.push(0);
        let application = wide(recorder.as_os_str())?;
        let directory = wide(
            recorder
                .parent()
                .ok_or_else(|| invalid("recorder has no directory"))?
                .as_os_str(),
        )?;
        let mut process = ProcessInformation::default();
        unsafe {
            check(CreateProcessW(
                application.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                1,
                CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                null(),
                directory.as_ptr(),
                &startup.startup,
                &mut process,
            ))?;
        }
        let process_handle = OwnedHandle(process.process);
        let _thread_handle = OwnedHandle(process.thread);
        // No job handle is inherited. Host exit/crash closes the sole handle
        // and terminates the complete recorder/SSH/Codex descendant tree.
        if unsafe { WaitForSingleObject(process_handle.0, INFINITE) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut exit = 0;
        unsafe {
            check(GetExitCodeProcess(process_handle.0, &mut exit))?;
        }
        Ok(exit as i32)
    }
}

fn main() {
    #[cfg(windows)]
    match windows::run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("recorder_host_failed: {error}");
            std::process::exit(1);
        }
    }
    #[cfg(not(windows))]
    std::process::exit(1);
}
