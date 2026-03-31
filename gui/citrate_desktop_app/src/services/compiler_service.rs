//! Solidity compiler service — Foundry (forge) integration for smart contract compilation.
//!
//! Data source: `forge build --json` subprocess for compilation, `forge --version` for
//! toolchain detection, and `out/<Contract>.sol/<Contract>.json` artifacts on disk.

use crate::error::AppError;
use crate::event_bus::{AppEvent, EventBus};
use crate::view_models::ide_view_models::{CompileError, CompileResult, ContractArtifact};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Backend trait
// ---------------------------------------------------------------------------

/// Backend trait for Solidity compilation via the Foundry toolchain.
#[async_trait::async_trait]
pub trait CompilerBackend: Send + Sync {
    /// Run `forge build` on the given project root and return a structured result.
    async fn compile_project(&self, project_root: &Path) -> Result<CompileResult, AppError>;

    /// Verify that forge/solc is available and return a version string (e.g. "forge 0.2.0").
    async fn check_toolchain(&self) -> Result<String, AppError>;

    /// Read the ABI JSON for a named contract from the forge output directory.
    async fn get_abi(
        &self,
        project_root: &Path,
        contract_name: &str,
    ) -> Result<String, AppError>;
}

// ---------------------------------------------------------------------------
// Real backend — invokes `forge` as a subprocess
// ---------------------------------------------------------------------------

/// Production backend that shells out to `forge` (Foundry).
pub struct ForgeCompilerBackend;

impl ForgeCompilerBackend {
    /// Parse forge stderr for diagnostic lines matching the pattern:
    /// `Error (XXXX): message --> file:line:col`
    /// `Warning (XXXX): message --> file:line:col`
    fn parse_diagnostics(stderr: &str) -> (Vec<CompileError>, Vec<CompileError>) {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        for line in stderr.lines() {
            let trimmed = line.trim();

            // Match "Error (...): ..." or "Warning (...): ..."
            let (severity, rest) = if let Some(rest) = trimmed.strip_prefix("Error ") {
                ("error", rest)
            } else if let Some(rest) = trimmed.strip_prefix("Warning ") {
                ("warning", rest)
            } else {
                continue;
            };

            // Extract the message (everything after "): ")
            let message = if let Some(idx) = rest.find("): ") {
                rest[idx + 3..].to_string()
            } else {
                rest.to_string()
            };

            // Look for location " --> file:line:col"
            let (file_path, line_num, col_num) = if let Some(arrow_idx) = message.find(" --> ") {
                let location_part = &message[arrow_idx + 5..];
                let msg_only = message[..arrow_idx].to_string();
                let (fp, ln, cn) = Self::parse_location(location_part);
                // Re-assign message to the part before " --> "
                // We'll reconstruct below
                let _ = msg_only;
                (fp, ln, cn)
            } else {
                (String::new(), 0, 0)
            };

            let clean_message = if let Some(arrow_idx) = message.find(" --> ") {
                message[..arrow_idx].to_string()
            } else {
                message
            };

            let diag = CompileError {
                file_path,
                line: line_num,
                column: col_num,
                severity: severity.to_string(),
                message: clean_message,
            };

            if severity == "error" {
                errors.push(diag);
            } else {
                warnings.push(diag);
            }
        }

        (errors, warnings)
    }

    /// Parse a "file:line:col" location string.
    fn parse_location(location: &str) -> (String, usize, usize) {
        let parts: Vec<&str> = location.splitn(3, ':').collect();
        match parts.len() {
            3 => {
                let file = parts[0].trim().to_string();
                let line = parts[1].trim().parse::<usize>().unwrap_or(0);
                let col = parts[2].trim().parse::<usize>().unwrap_or(0);
                (file, line, col)
            }
            2 => {
                let file = parts[0].trim().to_string();
                let line = parts[1].trim().parse::<usize>().unwrap_or(0);
                (file, line, 0)
            }
            _ => (location.trim().to_string(), 0, 0),
        }
    }

    /// Parse the JSON stdout from `forge build --json` to extract contract artifacts.
    fn parse_artifacts_from_json(stdout: &str) -> Vec<ContractArtifact> {
        let mut artifacts = Vec::new();

        let parsed: serde_json::Value = match serde_json::from_str(stdout) {
            Ok(v) => v,
            Err(_) => return artifacts,
        };

        // Forge JSON output has a "contracts" key mapping source files to contract data
        if let Some(contracts) = parsed.get("contracts").and_then(|c| c.as_object()) {
            for (_source_file, file_contracts) in contracts {
                if let Some(file_obj) = file_contracts.as_object() {
                    for (contract_name, contract_data) in file_obj {
                        let abi_json = contract_data
                            .get("abi")
                            .map(|a| a.to_string())
                            .unwrap_or_else(|| "[]".to_string());

                        let bytecode_hex = contract_data
                            .get("evm")
                            .and_then(|evm| evm.get("bytecode"))
                            .and_then(|bc| bc.get("object"))
                            .and_then(|obj| obj.as_str())
                            .unwrap_or("")
                            .to_string();

                        artifacts.push(ContractArtifact {
                            name: contract_name.clone(),
                            abi_json,
                            bytecode_hex,
                        });
                    }
                }
            }
        }

        artifacts
    }

    /// Artifact path for a contract: `<project_root>/out/<Name>.sol/<Name>.json`
    fn artifact_path(project_root: &Path, contract_name: &str) -> PathBuf {
        project_root
            .join("out")
            .join(format!("{contract_name}.sol"))
            .join(format!("{contract_name}.json"))
    }
}

#[async_trait::async_trait]
impl CompilerBackend for ForgeCompilerBackend {
    async fn compile_project(&self, project_root: &Path) -> Result<CompileResult, AppError> {
        let root = project_root.to_path_buf();

        let output = tokio::process::Command::new("forge")
            .arg("build")
            .arg("--json")
            .current_dir(&root)
            .output()
            .await
            .map_err(|e| {
                AppError::Compiler(format!(
                    "Failed to execute forge: {}. Is Foundry installed?",
                    e
                ))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        let (errors, warnings) = Self::parse_diagnostics(&stderr);
        let success = output.status.success() && errors.is_empty();

        let artifacts = if success {
            Self::parse_artifacts_from_json(&stdout)
        } else {
            Vec::new()
        };

        Ok(CompileResult {
            success,
            errors,
            warnings,
            artifacts,
        })
    }

    async fn check_toolchain(&self) -> Result<String, AppError> {
        let output = tokio::process::Command::new("forge")
            .arg("--version")
            .output()
            .await
            .map_err(|e| {
                AppError::Compiler(format!(
                    "Foundry toolchain not found: {}. Install with `curl -L https://foundry.paradigm.xyz | bash`",
                    e
                ))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(AppError::Compiler(format!(
                "forge --version failed: {}",
                stderr
            )));
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(version)
    }

    async fn get_abi(
        &self,
        project_root: &Path,
        contract_name: &str,
    ) -> Result<String, AppError> {
        let artifact_path = Self::artifact_path(project_root, contract_name);

        let content = tokio::fs::read_to_string(&artifact_path)
            .await
            .map_err(|e| {
                AppError::Compiler(format!(
                    "Cannot read artifact {}: {}. Run `forge build` first.",
                    artifact_path.display(),
                    e
                ))
            })?;

        let parsed: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
            AppError::Compiler(format!(
                "Invalid artifact JSON at {}: {}",
                artifact_path.display(),
                e
            ))
        })?;

        let abi = parsed
            .get("abi")
            .ok_or_else(|| {
                AppError::Compiler(format!(
                    "No 'abi' field in artifact for contract '{}'",
                    contract_name
                ))
            })?
            .to_string();

        Ok(abi)
    }
}

// ---------------------------------------------------------------------------
// Test-only backend — not compiled in release builds
// ---------------------------------------------------------------------------

/// Test-only in-memory compiler backend. Not available in release builds.
#[cfg(test)]
pub struct TestCompilerBackend {
    compile_result: RwLock<CompileResult>,
    toolchain_version: RwLock<Result<String, String>>,
    abis: RwLock<std::collections::HashMap<String, String>>,
}

#[cfg(test)]
impl Default for TestCompilerBackend {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
impl TestCompilerBackend {
    pub fn new() -> Self {
        Self {
            compile_result: RwLock::new(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                artifacts: Vec::new(),
            }),
            toolchain_version: RwLock::new(Ok("forge 0.2.0 (test)".to_string())),
            abis: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Configure the result that `compile_project` will return.
    pub async fn set_compile_result(&self, result: CompileResult) {
        *self.compile_result.write().await = result;
    }

    /// Configure the result that `check_toolchain` will return.
    pub async fn set_toolchain_result(&self, result: Result<String, String>) {
        *self.toolchain_version.write().await = result;
    }

    /// Register an ABI for a contract name.
    pub async fn set_abi(&self, contract_name: &str, abi_json: &str) {
        self.abis
            .write()
            .await
            .insert(contract_name.to_string(), abi_json.to_string());
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl CompilerBackend for TestCompilerBackend {
    async fn compile_project(&self, _project_root: &Path) -> Result<CompileResult, AppError> {
        Ok(self.compile_result.read().await.clone())
    }

    async fn check_toolchain(&self) -> Result<String, AppError> {
        self.toolchain_version
            .read()
            .await
            .clone()
            .map_err(AppError::Compiler)
    }

    async fn get_abi(
        &self,
        _project_root: &Path,
        contract_name: &str,
    ) -> Result<String, AppError> {
        self.abis
            .read()
            .await
            .get(contract_name)
            .cloned()
            .ok_or_else(|| {
                AppError::Compiler(format!(
                    "No artifact found for contract '{}'",
                    contract_name
                ))
            })
    }
}

// ---------------------------------------------------------------------------
// CompilerService — wraps a backend with caching, state, and events
// ---------------------------------------------------------------------------

/// Solidity compiler service with cached diagnostics and event publishing.
pub struct CompilerService {
    events: Arc<EventBus>,
    backend: Arc<dyn CompilerBackend>,
    last_result: Arc<RwLock<Option<CompileResult>>>,
    compiling: Arc<RwLock<bool>>,
}

impl CompilerService {
    /// Create with the real Forge backend (production).
    pub fn new(events: Arc<EventBus>) -> Self {
        Self {
            events,
            backend: Arc::new(ForgeCompilerBackend),
            last_result: Arc::new(RwLock::new(None)),
            compiling: Arc::new(RwLock::new(false)),
        }
    }

    /// Create with an injected backend (for testing or alternative compiler).
    pub fn with_backend(events: Arc<EventBus>, backend: Arc<dyn CompilerBackend>) -> Self {
        Self {
            events,
            backend,
            last_result: Arc::new(RwLock::new(None)),
            compiling: Arc::new(RwLock::new(false)),
        }
    }

    /// Compile the project at `project_root`. Caches the result and publishes
    /// `AppEvent::CompileCompleted` when finished.
    pub async fn compile(&self, project_root: &Path) -> Result<CompileResult, AppError> {
        {
            let mut flag = self.compiling.write().await;
            *flag = true;
        }

        let result = self.backend.compile_project(project_root).await;

        {
            let mut flag = self.compiling.write().await;
            *flag = false;
        }

        match result {
            Ok(compile_result) => {
                let event = AppEvent::CompileCompleted {
                    success: compile_result.success,
                    error_count: compile_result.errors.len(),
                    warning_count: compile_result.warnings.len(),
                };

                {
                    let mut cached = self.last_result.write().await;
                    *cached = Some(compile_result.clone());
                }

                self.events.publish(event);

                Ok(compile_result)
            }
            Err(e) => {
                // On backend failure, publish a failure event and cache an error result
                let error_result = CompileResult {
                    success: false,
                    errors: vec![CompileError {
                        file_path: String::new(),
                        line: 0,
                        column: 0,
                        severity: "error".to_string(),
                        message: e.to_string(),
                    }],
                    warnings: Vec::new(),
                    artifacts: Vec::new(),
                };

                self.events.publish(AppEvent::CompileCompleted {
                    success: false,
                    error_count: 1,
                    warning_count: 0,
                });

                {
                    let mut cached = self.last_result.write().await;
                    *cached = Some(error_result);
                }

                Err(e)
            }
        }
    }

    /// Check whether the Foundry toolchain is available. Returns the version string.
    pub async fn check_toolchain(&self) -> Result<String, AppError> {
        self.backend.check_toolchain().await
    }

    /// Read the ABI for a specific contract from forge output.
    pub async fn get_abi(
        &self,
        project_root: &Path,
        contract_name: &str,
    ) -> Result<String, AppError> {
        self.backend.get_abi(project_root, contract_name).await
    }

    /// Return cached errors and warnings from the last compilation.
    pub async fn get_diagnostics(&self) -> (Vec<CompileError>, Vec<CompileError>) {
        let cached = self.last_result.read().await;
        match cached.as_ref() {
            Some(result) => (result.errors.clone(), result.warnings.clone()),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Return whether a compilation is currently in progress.
    pub async fn is_compiling(&self) -> bool {
        *self.compiling.read().await
    }

    /// Return the last compile result, if any.
    pub async fn last_result(&self) -> Option<CompileResult> {
        self.last_result.read().await.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_events() -> Arc<EventBus> {
        Arc::new(EventBus::new())
    }

    fn test_backend() -> Arc<TestCompilerBackend> {
        Arc::new(TestCompilerBackend::new())
    }

    fn test_service_with(
        events: Arc<EventBus>,
        backend: Arc<TestCompilerBackend>,
    ) -> CompilerService {
        CompilerService::with_backend(events, backend)
    }

    fn sample_project_root() -> PathBuf {
        PathBuf::from("/tmp/test-foundry-project")
    }

    // ----- Successful compilation -----

    #[tokio::test]
    async fn test_compile_success_returns_ok() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                artifacts: vec![ContractArtifact {
                    name: "Token".to_string(),
                    abi_json: r#"[{"type":"function","name":"totalSupply"}]"#.to_string(),
                    bytecode_hex: "0x6080604052".to_string(),
                }],
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile should succeed");

        assert!(result.success);
        assert!(result.errors.is_empty());
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].name, "Token");
    }

    #[tokio::test]
    async fn test_compile_success_with_multiple_artifacts() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                artifacts: vec![
                    ContractArtifact {
                        name: "Token".to_string(),
                        abi_json: "[]".to_string(),
                        bytecode_hex: "0x01".to_string(),
                    },
                    ContractArtifact {
                        name: "Staking".to_string(),
                        abi_json: "[]".to_string(),
                        bytecode_hex: "0x02".to_string(),
                    },
                ],
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile should succeed");

        assert!(result.success);
        assert_eq!(result.artifacts.len(), 2);
    }

    // ----- Compilation with errors -----

    #[tokio::test]
    async fn test_compile_with_errors() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 10,
                    column: 5,
                    severity: "error".to_string(),
                    message: "undeclared identifier 'balances'".to_string(),
                }],
                warnings: Vec::new(),
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile returns Ok even on compile errors");

        assert!(!result.success);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].line, 10);
        assert!(result.errors[0].message.contains("undeclared identifier"));
    }

    #[tokio::test]
    async fn test_compile_with_multiple_errors() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![
                    CompileError {
                        file_path: "src/Token.sol".to_string(),
                        line: 10,
                        column: 5,
                        severity: "error".to_string(),
                        message: "undeclared identifier".to_string(),
                    },
                    CompileError {
                        file_path: "src/Token.sol".to_string(),
                        line: 20,
                        column: 1,
                        severity: "error".to_string(),
                        message: "type mismatch".to_string(),
                    },
                ],
                warnings: Vec::new(),
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile returns Ok with error diagnostics");

        assert!(!result.success);
        assert_eq!(result.errors.len(), 2);
    }

    // ----- Compilation with warnings -----

    #[tokio::test]
    async fn test_compile_with_warnings_only() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 15,
                    column: 3,
                    severity: "warning".to_string(),
                    message: "unused variable 'x'".to_string(),
                }],
                artifacts: vec![ContractArtifact {
                    name: "Token".to_string(),
                    abi_json: "[]".to_string(),
                    bytecode_hex: "0x01".to_string(),
                }],
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile with warnings succeeds");

        assert!(result.success);
        assert!(result.errors.is_empty());
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].severity, "warning");
    }

    #[tokio::test]
    async fn test_compile_with_errors_and_warnings() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 10,
                    column: 5,
                    severity: "error".to_string(),
                    message: "syntax error".to_string(),
                }],
                warnings: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 5,
                    column: 1,
                    severity: "warning".to_string(),
                    message: "SPDX license not provided".to_string(),
                }],
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("compile returns Ok with mixed diagnostics");

        assert!(!result.success);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.warnings.len(), 1);
    }

    // ----- ABI retrieval -----

    #[tokio::test]
    async fn test_get_abi_success() {
        let events = test_events();
        let backend = test_backend();
        let abi_json = r#"[{"type":"function","name":"transfer","inputs":[]}]"#;
        backend.set_abi("Token", abi_json).await;

        let svc = test_service_with(events, backend);
        let abi = svc
            .get_abi(&sample_project_root(), "Token")
            .await
            .expect("ABI retrieval should succeed");

        assert_eq!(abi, abi_json);
    }

    #[tokio::test]
    async fn test_get_abi_not_found() {
        let events = test_events();
        let backend = test_backend();

        let svc = test_service_with(events, backend);
        let result = svc.get_abi(&sample_project_root(), "NonExistent").await;

        assert!(result.is_err());
        let err_msg = result.expect_err("should be an error").to_string();
        assert!(err_msg.contains("NonExistent"));
    }

    // ----- Toolchain check -----

    #[tokio::test]
    async fn test_check_toolchain_available() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_toolchain_result(Ok("forge 0.3.0 (abc1234 2026-03-20)".to_string()))
            .await;

        let svc = test_service_with(events, backend);
        let version = svc
            .check_toolchain()
            .await
            .expect("toolchain check should succeed");

        assert!(version.contains("forge"));
    }

    #[tokio::test]
    async fn test_check_toolchain_not_found() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_toolchain_result(Err("forge not found in PATH".to_string()))
            .await;

        let svc = test_service_with(events, backend);
        let result = svc.check_toolchain().await;

        assert!(result.is_err());
        let err_msg = result.expect_err("should fail").to_string();
        assert!(err_msg.contains("forge not found"));
    }

    // ----- Empty project -----

    #[tokio::test]
    async fn test_compile_empty_project() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        let result = svc
            .compile(&sample_project_root())
            .await
            .expect("empty project compiles successfully");

        assert!(result.success);
        assert!(result.artifacts.is_empty());
        assert!(result.errors.is_empty());
    }

    // ----- is_compiling state transitions -----

    #[tokio::test]
    async fn test_is_compiling_initially_false() {
        let events = test_events();
        let backend = test_backend();
        let svc = test_service_with(events, backend);

        assert!(!svc.is_compiling().await);
    }

    #[tokio::test]
    async fn test_is_compiling_false_after_compile() {
        let events = test_events();
        let backend = test_backend();
        let svc = test_service_with(events, backend);

        svc.compile(&sample_project_root())
            .await
            .expect("compile succeeds");

        assert!(!svc.is_compiling().await, "should be false after compile finishes");
    }

    // ----- Event publishing -----

    #[tokio::test]
    async fn test_compile_publishes_event_on_success() {
        let events = test_events();
        let mut rx = events.subscribe();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 1,
                    column: 1,
                    severity: "warning".to_string(),
                    message: "unused".to_string(),
                }],
                artifacts: vec![ContractArtifact {
                    name: "Token".to_string(),
                    abi_json: "[]".to_string(),
                    bytecode_hex: "0x".to_string(),
                }],
            })
            .await;

        let svc = test_service_with(events, backend);
        svc.compile(&sample_project_root())
            .await
            .expect("compile succeeds");

        let event = rx.recv().await.expect("should receive compile event");
        match event {
            AppEvent::CompileCompleted {
                success,
                error_count,
                warning_count,
            } => {
                assert!(success);
                assert_eq!(error_count, 0);
                assert_eq!(warning_count, 1);
            }
            other => panic!("Expected CompileCompleted, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_compile_publishes_event_on_failure() {
        let events = test_events();
        let mut rx = events.subscribe();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![
                    CompileError {
                        file_path: "a.sol".to_string(),
                        line: 1,
                        column: 1,
                        severity: "error".to_string(),
                        message: "e1".to_string(),
                    },
                    CompileError {
                        file_path: "b.sol".to_string(),
                        line: 2,
                        column: 2,
                        severity: "error".to_string(),
                        message: "e2".to_string(),
                    },
                ],
                warnings: Vec::new(),
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        svc.compile(&sample_project_root())
            .await
            .expect("compile returns Ok with error diagnostics");

        let event = rx.recv().await.expect("should receive compile event");
        match event {
            AppEvent::CompileCompleted {
                success,
                error_count,
                warning_count,
            } => {
                assert!(!success);
                assert_eq!(error_count, 2);
                assert_eq!(warning_count, 0);
            }
            other => panic!("Expected CompileCompleted, got {:?}", other),
        }
    }

    // ----- Diagnostics caching -----

    #[tokio::test]
    async fn test_get_diagnostics_empty_before_compile() {
        let events = test_events();
        let backend = test_backend();
        let svc = test_service_with(events, backend);

        let (errors, warnings) = svc.get_diagnostics().await;
        assert!(errors.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn test_get_diagnostics_cached_after_compile() {
        let events = test_events();
        let backend = test_backend();
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 42,
                    column: 7,
                    severity: "error".to_string(),
                    message: "type error".to_string(),
                }],
                warnings: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 3,
                    column: 1,
                    severity: "warning".to_string(),
                    message: "license not set".to_string(),
                }],
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend);
        svc.compile(&sample_project_root())
            .await
            .expect("compile returns Ok");

        let (errors, warnings) = svc.get_diagnostics().await;
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].line, 42);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].message, "license not set");
    }

    #[tokio::test]
    async fn test_diagnostics_updated_on_recompile() {
        let events = test_events();
        let backend = test_backend();

        // First compile: 1 error
        backend
            .set_compile_result(CompileResult {
                success: false,
                errors: vec![CompileError {
                    file_path: "src/Token.sol".to_string(),
                    line: 1,
                    column: 1,
                    severity: "error".to_string(),
                    message: "first error".to_string(),
                }],
                warnings: Vec::new(),
                artifacts: Vec::new(),
            })
            .await;

        let svc = test_service_with(events, backend.clone());
        svc.compile(&sample_project_root())
            .await
            .expect("first compile");

        let (errors_1, _) = svc.get_diagnostics().await;
        assert_eq!(errors_1.len(), 1);
        assert_eq!(errors_1[0].message, "first error");

        // Second compile: success, no errors
        backend
            .set_compile_result(CompileResult {
                success: true,
                errors: Vec::new(),
                warnings: Vec::new(),
                artifacts: vec![ContractArtifact {
                    name: "Token".to_string(),
                    abi_json: "[]".to_string(),
                    bytecode_hex: "0x".to_string(),
                }],
            })
            .await;

        svc.compile(&sample_project_root())
            .await
            .expect("second compile");

        let (errors_2, _) = svc.get_diagnostics().await;
        assert!(errors_2.is_empty(), "diagnostics should be updated after recompile");
    }

    // ----- last_result -----

    #[tokio::test]
    async fn test_last_result_none_before_compile() {
        let events = test_events();
        let backend = test_backend();
        let svc = test_service_with(events, backend);

        assert!(svc.last_result().await.is_none());
    }

    #[tokio::test]
    async fn test_last_result_some_after_compile() {
        let events = test_events();
        let backend = test_backend();
        let svc = test_service_with(events, backend);

        svc.compile(&sample_project_root())
            .await
            .expect("compile succeeds");

        let last = svc.last_result().await;
        assert!(last.is_some());
        assert!(last.expect("should be Some").success);
    }

    // ----- Diagnostic parsing (ForgeCompilerBackend internals) -----

    #[test]
    fn test_parse_diagnostics_error_line() {
        let stderr = "Error (2314): undeclared identifier --> src/Token.sol:10:5\n";
        let (errors, warnings) = ForgeCompilerBackend::parse_diagnostics(stderr);
        assert_eq!(errors.len(), 1);
        assert!(warnings.is_empty());
        assert_eq!(errors[0].file_path, "src/Token.sol");
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].column, 5);
    }

    #[test]
    fn test_parse_diagnostics_warning_line() {
        let stderr = "Warning (1234): unused variable --> src/Token.sol:3:1\n";
        let (errors, warnings) = ForgeCompilerBackend::parse_diagnostics(stderr);
        assert!(errors.is_empty());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].file_path, "src/Token.sol");
        assert_eq!(warnings[0].line, 3);
    }

    #[test]
    fn test_parse_diagnostics_mixed() {
        let stderr = "\
Error (2314): undeclared identifier --> src/Token.sol:10:5
Warning (5678): unused variable --> src/Token.sol:3:1
Error (9999): type mismatch --> src/Staking.sol:20:12
Some other line that should be ignored
";
        let (errors, warnings) = ForgeCompilerBackend::parse_diagnostics(stderr);
        assert_eq!(errors.len(), 2);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn test_parse_diagnostics_empty_stderr() {
        let (errors, warnings) = ForgeCompilerBackend::parse_diagnostics("");
        assert!(errors.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn test_parse_diagnostics_no_location() {
        let stderr = "Error (1000): general compilation failure\n";
        let (errors, warnings) = ForgeCompilerBackend::parse_diagnostics(stderr);
        assert_eq!(errors.len(), 1);
        assert!(warnings.is_empty());
        // No location means file_path is empty, line/col are 0
        assert_eq!(errors[0].line, 0);
    }

    // ----- Location parsing -----

    #[test]
    fn test_parse_location_full() {
        let (file, line, col) = ForgeCompilerBackend::parse_location("src/Token.sol:42:8");
        assert_eq!(file, "src/Token.sol");
        assert_eq!(line, 42);
        assert_eq!(col, 8);
    }

    #[test]
    fn test_parse_location_no_col() {
        let (file, line, col) = ForgeCompilerBackend::parse_location("src/Token.sol:42");
        assert_eq!(file, "src/Token.sol");
        assert_eq!(line, 42);
        assert_eq!(col, 0);
    }

    #[test]
    fn test_parse_location_no_numbers() {
        let (file, line, col) = ForgeCompilerBackend::parse_location("src/Token.sol");
        assert_eq!(file, "src/Token.sol");
        assert_eq!(line, 0);
        assert_eq!(col, 0);
    }

    // ----- Artifact JSON parsing -----

    #[test]
    fn test_parse_artifacts_valid_json() {
        let json = r#"{
            "contracts": {
                "src/Token.sol": {
                    "Token": {
                        "abi": [{"type":"function","name":"transfer"}],
                        "evm": {
                            "bytecode": {
                                "object": "6080604052"
                            }
                        }
                    }
                }
            }
        }"#;
        let artifacts = ForgeCompilerBackend::parse_artifacts_from_json(json);
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].name, "Token");
        assert!(artifacts[0].abi_json.contains("transfer"));
        assert_eq!(artifacts[0].bytecode_hex, "6080604052");
    }

    #[test]
    fn test_parse_artifacts_invalid_json() {
        let artifacts = ForgeCompilerBackend::parse_artifacts_from_json("not json");
        assert!(artifacts.is_empty());
    }

    #[test]
    fn test_parse_artifacts_empty_contracts() {
        let json = r#"{"contracts": {}}"#;
        let artifacts = ForgeCompilerBackend::parse_artifacts_from_json(json);
        assert!(artifacts.is_empty());
    }

    // ----- Artifact path -----

    #[test]
    fn test_artifact_path_construction() {
        let root = PathBuf::from("/home/user/project");
        let path = ForgeCompilerBackend::artifact_path(&root, "Token");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/out/Token.sol/Token.json")
        );
    }

    // ----- Service construction -----

    #[test]
    fn test_new_creates_service() {
        let events = test_events();
        let _svc = CompilerService::new(events);
        // Verifies that new() compiles and returns without panic
    }

    #[test]
    fn test_with_backend_creates_service() {
        let events = test_events();
        let backend = test_backend();
        let _svc = CompilerService::with_backend(events, backend);
    }
}
