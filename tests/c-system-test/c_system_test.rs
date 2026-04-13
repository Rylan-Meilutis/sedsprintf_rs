mod c_system_test {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::thread;
    use std::time::{Duration, Instant};

    fn run_exe(root: &PathBuf, name: &str) {
        let exe = root.join("build").join(name);
        let mut child = Command::new(&exe)
            .current_dir(root)
            .spawn()
            .unwrap_or_else(|e| panic!("Failed to run {name}: {e}"));

        let timeout = Duration::from_secs(45);
        let start = Instant::now();
        let output = loop {
            if let Some(_status) = child
                .try_wait()
                .unwrap_or_else(|e| panic!("Failed to wait for {name}: {e}"))
            {
                break child
                    .wait_with_output()
                    .unwrap_or_else(|e| panic!("Failed to collect {name} output: {e}"));
            }

            if start.elapsed() >= timeout {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|e| panic!("Failed to collect timed out {name} output: {e}"));
                eprintln!(
                    "{} stdout:\n{}",
                    name,
                    String::from_utf8_lossy(&output.stdout)
                );
                eprintln!(
                    "{} stderr:\n{}",
                    name,
                    String::from_utf8_lossy(&output.stderr)
                );
                panic!("{name} timed out after {:?}", timeout);
            }

            thread::sleep(Duration::from_millis(50));
        };

        eprintln!(
            "{} stdout:\n{}",
            name,
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "{} stderr:\n{}",
            name,
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            output.status.success(),
            "{} failed with exit code {:?}",
            name,
            output.status.code()
        );
    }

    #[test]
    fn run_c_system_test() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("c-system-test");
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let build_dir = root.join("build");
        let macos_deployment_target = "26.0";

        if build_dir.exists() {
            fs::remove_dir_all(&build_dir).expect("Failed to remove stale c-system-test/build");
        }

        // Force-refresh Rust staticlib in non-python mode so C linking never
        // picks up stale pyo3-enabled artifacts from unrelated local builds.
        let mut prebuild = Command::new("python3");
        prebuild
            .arg("build.py")
            .arg("timesync")
            .arg("device_id=SYSTEM_TEST")
            .current_dir(&repo_root);
        if cfg!(target_os = "macos") {
            prebuild.env("MACOSX_DEPLOYMENT_TARGET", macos_deployment_target);
        }
        let status = prebuild
            .status()
            .expect("Failed to run build.py for c-system-test");
        assert!(status.success(), "build.py prebuild failed");

        let mut cmake_config = Command::new("cmake");
        cmake_config
            .arg("-S")
            .arg(".")
            .arg("-B")
            .arg(&build_dir)
            .arg("-DCMAKE_BUILD_TYPE=Debug")
            .current_dir(&root);
        if cfg!(target_os = "macos") {
            cmake_config.env("MACOSX_DEPLOYMENT_TARGET", macos_deployment_target);
        }
        let status = cmake_config.status().expect("Failed to config cmake build");
        assert!(status.success(), "CMake config failed");

        // Build the C project
        let mut cmake_build = Command::new("cmake");
        cmake_build
            .arg("--build")
            .arg(&build_dir)
            .current_dir(&root);
        if cfg!(target_os = "macos") {
            cmake_build.env("MACOSX_DEPLOYMENT_TARGET", macos_deployment_target);
        }
        let status = cmake_build.status().expect("Failed to invoke cmake build");
        assert!(status.success(), "CMake build failed");

        run_exe(&root, "c_system_test");
        run_exe(&root, "c_system_timesync_test");
        run_exe(&root, "c_system_board_topology_timesync_test");
        run_exe(&root, "c_system_multi_endpoint_topology_test");
    }
}
