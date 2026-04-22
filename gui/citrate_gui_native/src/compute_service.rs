//! Compute service — detect local hardware for the compute opt-in UX.
//!
//! P960-D WP-D.1 lives here. We detect:
//!
//! - CPU: core count, model name, frequency
//! - Memory: total RAM
//! - GPU (NVIDIA only): model, VRAM, compute capability, driver
//!
//! Design principles:
//!
//! - **Non-fatal everywhere.** Missing libnvml → no GPU reported,
//!   not a crash. This matches the user experience where a machine
//!   may have an AMD GPU, no GPU, or an integrated one.
//! - **Synchronous + fast.** Called once on Compute-panel open; no
//!   long-running probes.
//! - **No external daemons.** sysinfo and nvml-wrapper are direct
//!   OS/driver calls.
//!
//! The UI consumes `HardwareProfile` verbatim into a read-only card
//! above the opt-in toggle. Values are also persisted so the first
//! paint doesn't wait for the probe.

use serde::{Deserialize, Serialize};

/// Snapshot of detected local hardware. `None` on GPU when no NVIDIA
/// GPU is accessible; we don't currently detect AMD/Intel dGPUs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HardwareProfile {
    pub cpu_model: String,
    pub cpu_cores: u32,
    pub cpu_threads: u32,
    pub ram_gb: u32,
    pub gpu: Option<GpuInfo>,
    /// Unix seconds when this profile was detected. 0 if never.
    pub detected_at: u64,
}

/// NVIDIA GPU info. CUDA compute capability is presented as
/// "major.minor" (e.g. "8.9" for Ada Lovelace) since that's the
/// form model docs quote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub vram_gb: u32,
    pub compute_capability: String,
    pub driver_version: String,
}

impl HardwareProfile {
    /// Perform a fresh detection. Called sync because every probe
    /// is a one-shot driver call with microsecond latency.
    pub fn detect() -> Self {
        let (cpu_model, cpu_cores, cpu_threads) = detect_cpu();
        let ram_gb = detect_ram_gb();
        let gpu = detect_nvidia_gpu();
        let detected_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            cpu_model,
            cpu_cores,
            cpu_threads,
            ram_gb,
            gpu,
            detected_at,
        }
    }

    /// Short one-line summary for the UI card header.
    pub fn summary(&self) -> String {
        let cpu = format!("{} cores", self.cpu_cores);
        let ram = format!("{} GB RAM", self.ram_gb);
        match &self.gpu {
            Some(g) => format!("{} · {} VRAM · {} · {}", g.name, pretty_gb(g.vram_gb), cpu, ram),
            None => format!("No GPU detected · {} · {}", cpu, ram),
        }
    }
}

fn detect_cpu() -> (String, u32, u32) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_list(sysinfo::CpuRefreshKind::everything());
    let threads = sys.cpus().len() as u32;
    let cpu_model = sys
        .cpus()
        .first()
        .map(|c| c.brand().to_string())
        .unwrap_or_else(|| "Unknown CPU".to_string());
    // sysinfo's physical-core count is best-effort; fall back to thread
    // count halved if HT is suspected.
    let cores = sys
        .physical_core_count()
        .map(|c| c as u32)
        .unwrap_or_else(|| threads.max(1));
    (cpu_model, cores, threads)
}

fn detect_ram_gb() -> u32 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let bytes = sys.total_memory();
    // sysinfo 0.32 returns bytes. Round to the nearest GB.
    let gb = (bytes as f64 / 1_073_741_824.0).round();
    gb.max(0.0) as u32
}

/// NVIDIA GPU detection via NVML. Returns `None` when libnvml isn't
/// installed, no NVIDIA GPU is present, or the init fails for any
/// reason. Never panics.
fn detect_nvidia_gpu() -> Option<GpuInfo> {
    let nvml = match nvml_wrapper::Nvml::init() {
        Ok(n) => n,
        Err(e) => {
            tracing::debug!("NVML unavailable (ok for non-NVIDIA systems): {}", e);
            return None;
        }
    };
    let device = match nvml.device_by_index(0) {
        Ok(d) => d,
        Err(_) => return None,
    };
    let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".to_string());
    let vram_bytes = device.memory_info().map(|m| m.total).unwrap_or(0);
    let vram_gb = (vram_bytes as f64 / 1_073_741_824.0).round() as u32;
    let compute_capability = match device.cuda_compute_capability() {
        Ok(cc) => format!("{}.{}", cc.major, cc.minor),
        Err(_) => "unknown".to_string(),
    };
    let driver_version = nvml.sys_driver_version().unwrap_or_else(|_| "unknown".to_string());
    Some(GpuInfo {
        name,
        vram_gb,
        compute_capability,
        driver_version,
    })
}

fn pretty_gb(gb: u32) -> String {
    if gb == 0 { "< 1 GB".to_string() } else { format!("{} GB", gb) }
}

/// Compute-sharing user preferences. Persisted alongside the GUI
/// config.json so opt-in survives restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeSettings {
    /// Master switch. When false, allocation and schedule are
    /// ignored and we advertise nothing to the compute marketplace.
    pub enabled: bool,
    /// 0–100% of available GPU (or CPU when no GPU). Step 5.
    pub allocation_percent: u32,
    /// "always", "nights", or "weekends". Controls when we're
    /// willing to take jobs even when enabled.
    pub schedule: String,
}

impl Default for ComputeSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            allocation_percent: 50,
            schedule: "always".to_string(),
        }
    }
}

impl ComputeSettings {
    /// Path: `~/.local/share/citrate-gui/compute.json`. Kept
    /// separate from config.json so changes don't re-trigger node
    /// config reload.
    pub fn default_path() -> Option<std::path::PathBuf> {
        dirs::data_dir().map(|d| d.join("citrate-gui").join("compute.json"))
    }

    pub fn load(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str::<ComputeSettings>(&s).unwrap_or_default(),
            Err(_) => ComputeSettings::default(),
        }
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::other(format!("serialize: {}", e)))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Valid schedule tokens. Anything else normalizes to "always".
    pub fn normalize_schedule(s: &str) -> &'static str {
        match s {
            "nights" => "nights",
            "weekends" => "weekends",
            _ => "always",
        }
    }

    /// Human-readable description of the current schedule for the UI
    /// helper line below the schedule buttons.
    pub fn schedule_description(&self) -> &'static str {
        match Self::normalize_schedule(&self.schedule) {
            "nights" => "Share compute 10pm-6am local time",
            "weekends" => "Share compute on Saturday and Sunday",
            _ => "Share compute 24/7",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_settings_defaults_are_off() {
        let s = ComputeSettings::default();
        assert!(!s.enabled);
        assert_eq!(s.allocation_percent, 50);
        assert_eq!(s.schedule, "always");
    }

    #[test]
    fn schedule_normalization() {
        assert_eq!(ComputeSettings::normalize_schedule("nights"), "nights");
        assert_eq!(ComputeSettings::normalize_schedule("weekends"), "weekends");
        assert_eq!(ComputeSettings::normalize_schedule("always"), "always");
        assert_eq!(ComputeSettings::normalize_schedule("garbage"), "always");
        assert_eq!(ComputeSettings::normalize_schedule(""), "always");
    }

    #[test]
    fn schedule_description_matches_schedule() {
        let mut s = ComputeSettings::default();
        assert_eq!(s.schedule_description(), "Share compute 24/7");
        s.schedule = "nights".into();
        assert!(s.schedule_description().contains("10pm"));
        s.schedule = "weekends".into();
        assert!(s.schedule_description().contains("Saturday"));
    }

    #[test]
    fn settings_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "citrate-compute-test-{}.json",
            std::process::id()
        ));
        let s = ComputeSettings {
            enabled: true,
            allocation_percent: 75,
            schedule: "nights".into(),
        };
        s.save(&tmp).expect("save ok");
        let loaded = ComputeSettings::load(&tmp);
        assert!(loaded.enabled);
        assert_eq!(loaded.allocation_percent, 75);
        assert_eq!(loaded.schedule, "nights");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn hardware_profile_summary_never_empty() {
        // Note: real detection may yield "Unknown CPU" in CI sandbox
        // so we only assert non-empty + correct form.
        let hw = HardwareProfile::detect();
        let summary = hw.summary();
        assert!(!summary.is_empty());
        assert!(summary.contains("cores") || summary.contains("GPU"));
    }

    #[test]
    fn pretty_gb_handles_zero() {
        assert_eq!(pretty_gb(0), "< 1 GB");
        assert_eq!(pretty_gb(24), "24 GB");
    }
}
