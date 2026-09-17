use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};

pub(super) struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: null pointers request an unnamed job with a non-inheritable handle.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new handle owned by this function.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the handle is live and the buffer and size match the information class.
        let result = unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("completion worker exited"))?;
        // SAFETY: both handles are live. The worker waits for the parent before it starts helpers.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub(super) async fn start_worker(mut child: Child) -> io::Result<(Job, Child)> {
    let job = Job::new()?;
    job.assign(&child)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing worker input"))?;
    stdin.write_all(&[1]).await?;
    drop(stdin);
    Ok((job, child))
}

pub(super) fn wait_for_parent() -> bool {
    let mut ready = [0];
    io::stdin().read_exact(&mut ready).is_ok() && ready == [1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    #[test]
    #[ignore = "child process for the Job Object test"]
    fn worker() {
        let Ok(path) = std::env::var("SOFKA_JOB_TEST_PID") else {
            return;
        };
        if std::env::var_os("SOFKA_JOB_TEST_LEAF").is_some() {
            std::fs::write(path, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        assert!(wait_for_parent());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "completion::windows::tests::worker"])
            .env("SOFKA_JOB_TEST_LEAF", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = child.wait();
    }

    #[tokio::test]
    async fn timeout_kills_worker_and_descendant() {
        let path = std::env::temp_dir().join(format!("sofka-job-test-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--ignored", "--exact", "completion::windows::tests::worker"])
            .env("SOFKA_JOB_TEST_PID", &path)
            .env_remove("SOFKA_JOB_TEST_LEAF")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (job, child) = start_worker(command.spawn().unwrap()).await.unwrap();
        let descendant = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        // SAFETY: OpenProcess opens a synchronization handle for the test descendant.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, descendant) };
        assert!(!handle.is_null());
        // SAFETY: OpenProcess returned a new handle owned by this test.
        let descendant = unsafe { OwnedHandle::from_raw_handle(handle) };
        assert!(
            tokio::time::timeout(Duration::from_millis(50), child.wait_with_output())
                .await
                .is_err()
        );
        drop(job);
        // SAFETY: the owned handle stays live while Windows waits for process termination.
        assert_eq!(
            unsafe { WaitForSingleObject(descendant.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
        std::fs::remove_file(path).unwrap();
    }
}
