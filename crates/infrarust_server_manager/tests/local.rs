#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Write;

use infrarust_config::LocalManagerConfig;
use infrarust_server_manager::{LocalProvider, ProviderStatus, ServerProvider};

fn make_mock_script(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let f = std::fs::File::open(&path).unwrap();
        f.sync_all().unwrap();
    }
    path
}

fn script_name() -> &'static str {
    if cfg!(windows) {
        "server.ps1"
    } else {
        "server.sh"
    }
}

fn shell() -> &'static str {
    if cfg!(windows) { "powershell" } else { "bash" }
}

fn shell_args(script_path: &std::path::Path) -> Vec<String> {
    let script = script_path.to_str().unwrap().to_string();
    if cfg!(windows) {
        vec![
            "-NoProfile".to_string(),
            "-ExecutionPolicy".to_string(),
            "Bypass".to_string(),
            "-File".to_string(),
            script,
        ]
    } else {
        vec![script]
    }
}

fn make_config(script_path: &std::path::Path, working_dir: &std::path::Path) -> LocalManagerConfig {
    LocalManagerConfig {
        command: shell().to_string(),
        working_dir: working_dir.to_path_buf(),
        args: shell_args(script_path),
        ready_pattern: r#"For help, type "help""#.to_string(),
        shutdown_timeout: std::time::Duration::from_secs(5),
        shutdown_after: None,
        start_timeout: std::time::Duration::from_secs(10),
        poll_interval: std::time::Duration::from_secs(5),
    }
}

/// Windows PowerShell writes the success stream as UTF-16 unless told otherwise,
/// which hides the ready pattern from the provider's line reader.
fn script(body: &str) -> String {
    if cfg!(windows) {
        format!(
            "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding $false\n\
             $OutputEncoding = [Console]::OutputEncoding\n\
             {body}"
        )
    } else {
        format!("#!/bin/bash\n{body}")
    }
}

fn ready_line() -> &'static str {
    if cfg!(windows) {
        "[Console]::Out.WriteLine('For help, type \"help\"')\n[Console]::Out.Flush()"
    } else {
        "echo 'For help, type \"help\"'"
    }
}

fn wait_for_stop() -> &'static str {
    if cfg!(windows) {
        // [Console]::In does not see a redirected stdin and exits the script.
        "$reader = New-Object System.IO.StreamReader ([Console]::OpenStandardInput())\n\
         while ($null -ne ($line = $reader.ReadLine())) {\n\
             if ($line -eq \"stop\") { exit 0 }\n\
         }\n\
         exit 0"
    } else {
        "while read -r line; do\n    if [ \"$line\" = \"stop\" ]; then\n        exit 0\n    fi\ndone"
    }
}

async fn wait_for_status(provider: &LocalProvider, expected: ProviderStatus) {
    let mut last = None;
    for _ in 0..50 {
        let status = provider.check_status().await.unwrap();
        if status == expected {
            return;
        }
        last = Some(status);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("expected {expected:?}, last observed {last:?}");
}

#[tokio::test]
async fn test_check_status_not_started() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), script_name(), &script("echo done\n"));
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    let status = provider.check_status().await.unwrap();
    assert_eq!(status, ProviderStatus::Stopped);
}

#[tokio::test]
async fn test_start_detects_ready_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let loading = if cfg!(windows) {
        "Write-Output \"Loading...\""
    } else {
        "echo \"Loading...\""
    };
    let pause = if cfg!(windows) {
        "Start-Sleep -Milliseconds 200"
    } else {
        "sleep 0.2"
    };
    let script = make_mock_script(
        dir.path(),
        script_name(),
        &script(&format!(
            "{loading}\n{pause}\n{}\n{}",
            ready_line(),
            wait_for_stop()
        )),
    );
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();

    // Wait for the ready pattern to be detected
    let mut detected = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = provider.check_status().await.unwrap();
        if status == ProviderStatus::Running {
            detected = true;
            break;
        }
    }
    assert!(detected, "ready pattern should have been detected");

    // Stop the process
    provider.stop().await.unwrap();

    let status = provider.check_status().await.unwrap();
    assert_eq!(status, ProviderStatus::Stopped);
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_child_process_killed_when_provider_dropped() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("server.pid");
    let script_body = format!(
        "#!/bin/bash\necho $$ > {}\necho 'For help, type \"help\"'\nwhile true; do sleep 1; done\n",
        pid_file.display()
    );
    let script = make_mock_script(dir.path(), "server.sh", &script_body);
    let config = make_config(&script, dir.path());

    let provider = LocalProvider::new(config);
    provider.start().await.unwrap();

    let mut pid = None;
    for _ in 0..100 {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(p) = s.trim().parse::<u32>()
        {
            pid = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = pid.expect("script should record its PID");
    assert!(process_is_alive(pid), "process must be alive before drop");

    drop(provider);

    let mut killed = false;
    for _ in 0..100 {
        if !process_is_alive(pid) {
            killed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        killed,
        "child process {pid} must be killed when the provider is dropped without stop()"
    );
}

#[tokio::test]
async fn test_check_status_starting() {
    let dir = tempfile::tempdir().unwrap();
    // Script that never outputs the ready pattern
    let loading = if cfg!(windows) {
        "Write-Output \"Loading...\""
    } else {
        "echo \"Loading...\""
    };
    let body = script(&format!("{loading}\n{}", wait_for_stop()));
    let script = make_mock_script(dir.path(), script_name(), &body);
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let status = provider.check_status().await.unwrap();
    assert_eq!(status, ProviderStatus::Starting);

    // Cleanup
    provider.stop().await.unwrap();
}

#[tokio::test]
async fn test_unexpected_exit_code_reports_crashed() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), script_name(), &script("exit 1\n"));
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();
    wait_for_status(&provider, ProviderStatus::Crashed).await;
}

#[tokio::test]
async fn test_clean_self_exit_reports_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), script_name(), &script("exit 0\n"));
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();
    wait_for_status(&provider, ProviderStatus::Stopped).await;
}

#[tokio::test]
async fn test_stop_timeout_kills() {
    let dir = tempfile::tempdir().unwrap();
    // Script that ignores stop command
    let body = if cfg!(windows) {
        format!("{}\nStart-Sleep -Seconds 60\n", ready_line())
    } else {
        format!("{}\ntrap '' TERM INT\nsleep 60\n", ready_line())
    };
    let script = make_mock_script(dir.path(), script_name(), &script(&body));
    let mut config = make_config(&script, dir.path());
    config.shutdown_timeout = std::time::Duration::from_secs(1);
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Stop should timeout and kill
    provider.stop().await.unwrap();

    let status = provider.check_status().await.unwrap();
    assert_eq!(status, ProviderStatus::Stopped);
}

#[tokio::test]
async fn test_double_start_fails() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), script_name(), &script(wait_for_stop()));
    let config = make_config(&script, dir.path());
    let provider = LocalProvider::new(config);

    provider.start().await.unwrap();
    let error = provider.start().await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "process error: process is already running"
    );

    // Cleanup
    provider.stop().await.unwrap();
}

#[tokio::test]
async fn test_stop_without_a_process_fails() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), "server.sh", "#!/bin/bash\necho done\n");
    let provider = LocalProvider::new(make_config(&script, dir.path()));

    let error = provider.stop().await.unwrap_err();
    assert_eq!(error.to_string(), "process error: process is not running");
}
