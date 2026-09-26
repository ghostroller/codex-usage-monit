//! Atomic job membership closes the suspended-child crash window.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    mem::{size_of, size_of_val},
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    process::Command,
    ptr,
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    System::{
        JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
        Threading::*,
    },
};

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}
fn check(success: i32) -> Result<()> {
    ensure!(
        success != 0,
        "machine_process_failed: {}",
        io::Error::last_os_error()
    );
    Ok(())
}
fn inherited(file: &File) -> Result<Handle> {
    let mut handle = ptr::null_mut();
    unsafe {
        check(DuplicateHandle(
            GetCurrentProcess(),
            file.as_raw_handle(),
            GetCurrentProcess(),
            &mut handle,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        ))?;
    }
    Ok(Handle(handle))
}

struct Attributes(Vec<usize>);
impl Attributes {
    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }
}
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

pub(super) struct Child {
    process: Handle,
    // Never inherited: closing the sole job handle kills all descendants.
    job: Handle,
    pid: u32,
}
impl Child {
    pub(super) fn id(&self) -> u32 {
        self.pid
    }
    pub(super) fn try_wait(&self) -> Result<Option<u32>> {
        match unsafe { WaitForSingleObject(self.process.0, 0) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                unsafe {
                    check(GetExitCodeProcess(self.process.0, &mut code))?;
                }
                Ok(Some(code))
            }
            _ => Err(io::Error::last_os_error()).context("machine_process_wait_failed"),
        }
    }
    pub(super) fn terminate(&self) -> Result<()> {
        unsafe {
            check(TerminateJobObject(self.job.0, 1))?;
        }
        ensure!(
            unsafe { WaitForSingleObject(self.process.0, 10000) } == WAIT_OBJECT_0,
            "machine_process_stop_failed: recorder did not exit after job termination"
        );
        Ok(())
    }
}

fn wide(value: &OsStr) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    ensure!(
        !value.contains(&0),
        "machine_process_invalid: NUL in process argument"
    );
    value.push(0);
    Ok(value)
}
fn quote(value: &OsStr) -> Vec<u16> {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for value in value.encode_wide() {
        if value == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let count = if value == b'"' as u16 {
            slashes * 2 + 1
        } else {
            slashes
        };
        result.extend(std::iter::repeat_n(b'\\' as u16, count));
        result.push(value);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    result.push(b'"' as u16);
    result
}
fn environment_key(value: &OsStr) -> Vec<u16> {
    value
        .encode_wide()
        .map(|unit| {
            if (b'a' as u16..=b'z' as u16).contains(&unit) {
                unit - 32
            } else {
                unit
            }
        })
        .collect()
}
fn environment(command: &Command) -> Result<Vec<u16>> {
    // These commands intentionally inherit SCM's environment, then apply the
    // explicit service-account/profile overrides. env_clear is not used.
    let mut values: std::collections::BTreeMap<Vec<u16>, (OsString, OsString)> =
        std::env::vars_os()
            .map(|(key, value)| (environment_key(&key), (key, value)))
            .collect();
    for (key, value) in command.get_envs() {
        let identity = environment_key(key);
        if let Some(value) = value {
            values.insert(identity, (key.to_owned(), value.to_owned()));
        } else {
            values.remove(&identity);
        }
    }
    let mut block = Vec::new();
    for (_, (key, value)) in values {
        let key = wide(&key)?;
        block.extend_from_slice(&key[..key.len() - 1]);
        block.push(b'=' as u16);
        block.extend(wide(&value)?);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

pub(super) fn spawn(command: &Command, log: &File) -> Result<Child> {
    let stdin = inherited(&File::open("NUL")?)?;
    let log = inherited(log)?;
    let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    ensure!(
        !job.is_null(),
        "machine_job_failed: {}",
        io::Error::last_os_error()
    );
    let job = Handle(job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        check(SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        ))?;
    }
    let mut bytes = 0;
    unsafe {
        InitializeProcThreadAttributeList(ptr::null_mut(), 2, 0, &mut bytes);
    }
    ensure!(
        bytes > 0,
        "machine_job_attributes_failed: {}",
        io::Error::last_os_error()
    );
    let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    unsafe {
        check(InitializeProcThreadAttributeList(
            storage.as_mut_ptr().cast(),
            2,
            0,
            &mut bytes,
        ))?;
    }
    let mut attributes = Attributes(storage);
    let mut handles = [stdin.0, log.0];
    let mut jobs = [job.0];
    unsafe {
        check(UpdateProcThreadAttribute(
            attributes.pointer(),
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_mut_ptr().cast(),
            size_of_val(&handles),
            ptr::null_mut(),
            ptr::null_mut(),
        ))?;
        check(UpdateProcThreadAttribute(
            attributes.pointer(),
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_mut_ptr().cast(),
            size_of_val(&jobs),
            ptr::null_mut(),
            ptr::null_mut(),
        ))?;
    }
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.0;
    startup.StartupInfo.hStdOutput = log.0;
    startup.StartupInfo.hStdError = log.0;
    startup.lpAttributeList = attributes.pointer();
    let application = wide(command.get_program())?;
    let directory = command
        .get_current_dir()
        .map(|path| wide(path.as_os_str()))
        .transpose()?;
    let mut line = quote(command.get_program());
    for argument in command.get_args() {
        line.push(b' ' as u16);
        line.extend(quote(argument));
    }
    ensure!(
        !line.contains(&0) && line.len() < 32767,
        "machine_process_invalid: oversized or invalid command"
    );
    line.push(0);
    let environment = environment(command)?;
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        check(CreateProcessW(
            application.as_ptr(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            directory.as_ref().map_or(ptr::null(), |path| path.as_ptr()),
            &startup.StartupInfo,
            &mut information,
        ))?;
    }
    let _thread = Handle(information.hThread);
    Ok(Child {
        process: Handle(information.hProcess),
        job,
        pid: information.dwProcessId,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, Write},
        process::Stdio,
        time::Duration,
    };
    const ROLE: &str = "CODEX_USAGE_MONIT_SCM_PROCESS_FIXTURE";
    fn block() {
        let (_sender, receiver) = std::sync::mpsc::channel::<()>();
        let _ = receiver.recv();
    }
    #[test]
    fn scm_child_fixture() {
        if std::env::var(ROLE).as_deref() == Ok("child") {
            block();
        }
    }
    #[test]
    fn scm_parent_fixture() {
        if std::env::var(ROLE).as_deref() != Ok("parent") {
            return;
        }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "windows_scm::process::tests::scm_child_fixture",
                "--nocapture",
            ])
            .env(ROLE, "child");
        let log =
            File::create(std::env::var_os("CODEX_USAGE_MONIT_SCM_FIXTURE_LOG").unwrap()).unwrap();
        let child = spawn(&command, &log).unwrap();
        println!("SCM_CHILD_PID {}", child.id());
        io::stdout().flush().unwrap();
        block();
    }
    struct Parent(std::process::Child);
    impl Drop for Parent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn scm_atomic_job_kills_child_when_parent_is_killed() {
        let temporary = tempfile::tempdir().unwrap();
        let mut parent = Parent(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows_scm::process::tests::scm_parent_fixture",
                    "--nocapture",
                ])
                .env(ROLE, "parent")
                .env(
                    "CODEX_USAGE_MONIT_SCM_FIXTURE_LOG",
                    temporary.path().join("child.log"),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = parent.0.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(pid) = line
                    .strip_prefix("SCM_CHILD_PID ")
                    .and_then(|value| value.parse::<u32>().ok())
                {
                    let _ = sender.send(pid);
                    break;
                }
            }
        });
        let pid = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("atomic spawn must publish child PID");
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!process.is_null(), "fixture child must still be alive");
        let process = Handle(process);
        assert_eq!(unsafe { WaitForSingleObject(process.0, 0) }, WAIT_TIMEOUT);
        parent.0.kill().unwrap();
        parent.0.wait().unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(process.0, 5000) },
            WAIT_OBJECT_0,
            "closing the crashed parent's sole job handle must terminate the child"
        );
    }
}
