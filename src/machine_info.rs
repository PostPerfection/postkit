const UNKNOWN: &str = "unknown";
const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;
#[cfg(not(target_os = "macos"))]
const NVIDIA_SMI: &str = "nvidia-smi";
#[cfg(not(target_os = "macos"))]
const NVIDIA_SMI_GPU_QUERY: [&str; 2] = [
    "--query-gpu=name,driver_version,memory.total",
    "--format=csv,noheader",
];

#[derive(Default)]
struct Machine {
    os: Option<String>,
    cpu_model: Option<String>,
    cpu_count: Option<usize>,
    memory_bytes: Option<u64>,
}

pub fn machine_info_lines(accelerator_requested: bool) -> Vec<String> {
    let mut lines = vec![machine_line(&read_machine())];
    if accelerator_requested {
        lines.extend(gpu_lines(query_gpus().as_deref()));
    }
    lines
}

fn machine_line(machine: &Machine) -> String {
    let cpu = match (&machine.cpu_model, machine.cpu_count) {
        (Some(model), _) => model.clone(),
        (None, Some(count)) => format!("{count} CPUs"),
        (None, None) => UNKNOWN.to_string(),
    };
    let memory = machine.memory_bytes.map_or(UNKNOWN.to_string(), |bytes| {
        format!("{:.1} GiB", bytes as f64 / BYTES_PER_GIB)
    });
    format!(
        "Machine: {}, {cpu}, {memory}",
        machine.os.as_deref().unwrap_or(UNKNOWN)
    )
}

// None means nvidia-smi could not run
fn gpu_lines(nvidia_smi_output: Option<&str>) -> Vec<String> {
    let Some(output) = nvidia_smi_output else {
        return vec![format!("GPU: {UNKNOWN}")];
    };
    let lines: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(gpu_line)
        .collect();
    if lines.is_empty() {
        return vec!["GPU: none found".to_string()];
    }
    lines
}

fn gpu_line(nvidia_smi_row: &str) -> String {
    let fields: Vec<&str> = nvidia_smi_row.split(',').map(str::trim).collect();
    match fields.as_slice() {
        [name, driver, memory] => format!("GPU: {name}, driver {driver}, {memory}"),
        _ => format!("GPU: {nvidia_smi_row}"),
    }
}

#[cfg(target_os = "macos")]
fn query_gpus() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn query_gpus() -> Option<String> {
    let output = std::process::Command::new(NVIDIA_SMI)
        .args(NVIDIA_SMI_GPU_QUERY)
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(String::new());
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn cpu_count() -> Option<usize> {
    std::thread::available_parallelism()
        .ok()
        .map(std::num::NonZeroUsize::get)
}

#[cfg(target_os = "linux")]
fn read_machine() -> Machine {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    Machine {
        os: read("/etc/os-release").and_then(|text| os_release_name(&text)),
        cpu_model: read("/proc/cpuinfo").and_then(|text| cpuinfo_model(&text)),
        cpu_count: cpu_count(),
        memory_bytes: read("/proc/meminfo").and_then(|text| meminfo_total_bytes(&text)),
    }
}

#[cfg(target_os = "macos")]
fn read_machine() -> Machine {
    let product = command_stdout("sw_vers", &["-productName"]);
    let version = command_stdout("sw_vers", &["-productVersion"]);
    let os = match (product, version) {
        (Some(product), Some(version)) => Some(format!("{product} {version}")),
        (product, version) => product.or(version),
    };
    Machine {
        os,
        cpu_model: command_stdout("sysctl", &["-n", "machdep.cpu.brand_string"]),
        cpu_count: cpu_count(),
        memory_bytes: command_stdout("sysctl", &["-n", "hw.memsize"])
            .and_then(|bytes| bytes.parse().ok()),
    }
}

#[cfg(windows)]
const WINDOWS_MACHINE_QUERY: &str = "$os = Get-CimInstance Win32_OperatingSystem; \
     $os.Caption + ' ' + $os.Version; \
     (Get-CimInstance Win32_Processor | Select-Object -First 1).Name; \
     (Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory";

#[cfg(windows)]
fn read_machine() -> Machine {
    let answer = command_stdout(
        "powershell",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_MACHINE_QUERY,
        ],
    )
    .unwrap_or_default();
    let mut lines = answer
        .lines()
        .map(|line| Some(line.trim()).filter(|line| !line.is_empty()));
    let mut next = || lines.next().flatten().map(str::to_string);
    Machine {
        os: next(),
        cpu_model: next(),
        cpu_count: cpu_count(),
        memory_bytes: next().and_then(|bytes| bytes.parse().ok()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_machine() -> Machine {
    Machine {
        cpu_count: cpu_count(),
        ..Machine::default()
    }
}

#[cfg(any(target_os = "macos", windows))]
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(target_os = "linux")]
fn os_release_name(os_release: &str) -> Option<String> {
    let value = |key: &str| {
        os_release.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix('=')?;
            Some(value.trim().trim_matches('"').to_string())
        })
    };
    value("PRETTY_NAME").or_else(|| {
        let name = value("NAME")?;
        Some(match value("VERSION_ID") {
            Some(version) => format!("{name} {version}"),
            None => name,
        })
    })
}

#[cfg(target_os = "linux")]
fn cpuinfo_model(cpuinfo: &str) -> Option<String> {
    cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == "model name").then(|| value.trim().to_string())
    })
}

#[cfg(target_os = "linux")]
fn meminfo_total_bytes(meminfo: &str) -> Option<u64> {
    const BYTES_PER_KIB: u64 = 1024;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kibibytes: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kibibytes * BYTES_PER_KIB)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIXTEEN_GIB: u64 = 16 * 1024 * 1024 * 1024;

    #[test]
    fn the_machine_line_names_the_os_cpu_and_memory() {
        let machine = Machine {
            os: Some("Fedora Linux 43 (Workstation Edition)".to_string()),
            cpu_model: Some("AMD Ryzen 9 7950X 16-Core Processor".to_string()),
            cpu_count: Some(32),
            memory_bytes: Some(SIXTEEN_GIB),
        };
        assert_eq!(
            machine_line(&machine),
            "Machine: Fedora Linux 43 (Workstation Edition), AMD Ryzen 9 7950X 16-Core \
             Processor, 16.0 GiB"
        );
    }

    #[test]
    fn the_machine_line_counts_cpus_without_a_model_and_says_unknown_for_the_rest() {
        let machine = Machine {
            cpu_count: Some(8),
            ..Machine::default()
        };
        assert_eq!(machine_line(&machine), "Machine: unknown, 8 CPUs, unknown");
        assert_eq!(
            machine_line(&Machine::default()),
            "Machine: unknown, unknown, unknown"
        );
    }

    #[test]
    fn each_gpu_nvidia_smi_lists_gets_a_line() {
        let output = "NVIDIA GeForce RTX 3060 Laptop GPU, 580.178.04, 6144 MiB\n\
                      NVIDIA RTX A4000, 580.178.04, 16376 MiB\n";
        assert_eq!(
            gpu_lines(Some(output)),
            [
                "GPU: NVIDIA GeForce RTX 3060 Laptop GPU, driver 580.178.04, 6144 MiB",
                "GPU: NVIDIA RTX A4000, driver 580.178.04, 16376 MiB",
            ]
        );
    }

    #[test]
    fn no_answer_from_nvidia_smi_is_none_found_and_no_nvidia_smi_is_unknown() {
        assert_eq!(gpu_lines(Some("\n")), ["GPU: none found"]);
        assert_eq!(gpu_lines(None), ["GPU: unknown"]);
    }

    #[test]
    fn the_gpu_line_keeps_a_row_it_cannot_split() {
        assert_eq!(
            gpu_lines(Some("No devices were found")),
            ["GPU: No devices were found"]
        );
    }

    #[test]
    fn the_gpu_lines_follow_the_machine_line_only_when_the_accelerator_is_requested() {
        let without = machine_info_lines(false);
        assert_eq!(without.len(), 1);
        assert!(without[0].starts_with("Machine: "), "{without:?}");
        let with = machine_info_lines(true);
        assert!(with.len() >= 2, "{with:?}");
        assert!(with[1..].iter().all(|line| line.starts_with("GPU: ")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_files_give_the_os_cpu_model_and_memory() {
        let os_release = "NAME=\"Fedora Linux\"\nVERSION_ID=43\n\
                          PRETTY_NAME=\"Fedora Linux 43 (Workstation Edition)\"\n";
        assert_eq!(
            os_release_name(os_release).as_deref(),
            Some("Fedora Linux 43 (Workstation Edition)")
        );
        assert_eq!(
            os_release_name("NAME=Debian\nVERSION_ID=\"13\"\n").as_deref(),
            Some("Debian 13")
        );
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\n\
                       model name\t: Intel(R) Core(TM) i7-12700H\nprocessor\t: 1\n\
                       model name\t: Intel(R) Core(TM) i7-12700H\n";
        assert_eq!(
            cpuinfo_model(cpuinfo).as_deref(),
            Some("Intel(R) Core(TM) i7-12700H")
        );
        assert_eq!(cpuinfo_model("processor\t: 0\nBogoMIPS\t: 50.00\n"), None);
        assert_eq!(
            meminfo_total_bytes("MemTotal:       16777216 kB\nMemFree:  1 kB\n"),
            Some(SIXTEEN_GIB)
        );
    }
}
