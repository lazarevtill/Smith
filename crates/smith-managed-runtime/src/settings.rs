use crate::{Artifact, Failure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlashAttention {
    On,
    Off,
    Auto,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvType {
    F16,
    Q8_0,
    Q4_0,
}
impl KvType {
    fn arg(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8_0 => "q8_0",
            Self::Q4_0 => "q4_0",
        }
    }
}

/// No raw argv or environment entrypoint. Absence preserves runtime defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvancedSettings {
    pub prefill_threads: Option<u16>,
    pub batch_size: Option<u32>,
    pub micro_batch_size: Option<u32>,
    pub flash_attention: Option<FlashAttention>,
    pub cache_type_k: Option<KvType>,
    pub cache_type_v: Option<KvType>,
    pub mmap: Option<bool>,
    pub mlock: Option<bool>,
}

/// Only produced from a successful bounded `--help` command on the exact
/// selected executable. Serializable for review, never deserializable authority.
/// CLI syntax evidence does not prove inference or hardware support.
#[derive(Clone, Debug, Serialize)]
pub struct RuntimeCompatibility {
    executable_path: PathBuf,
    executable_sha256: String,
    flags: BTreeSet<String>,
    flash_modes_reported: bool,
    kv_types_reported: BTreeSet<String>,
    load_modes_reported: bool,
    unix_socket_reported: bool,
    fixed_values_reported: bool,
    pub evidence: &'static str,
}
impl RuntimeCompatibility {
    pub(crate) fn observed(artifact: &Artifact, help: &str, successful: bool) -> Self {
        let mut flags = BTreeSet::new();
        if successful {
            for word in help.split_whitespace() {
                let word = word.trim_matches(|c: char| matches!(c, ',' | '(' | ')' | '[' | ']'));
                if word.starts_with("--")
                    && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    flags.insert(word.into());
                }
            }
        }
        let flash_help = help_section(help, "--flash-attn");
        let flash_modes_reported = successful
            && ["on", "off", "auto"]
                .iter()
                .all(|word| has_word(&flash_help, word));
        let key_help = help_section(help, "--cache-type-k");
        let value_help = help_section(help, "--cache-type-v");
        let kv_types_reported = ["f16", "q8_0", "q4_0"]
            .into_iter()
            .filter(|kind| successful && has_word(&key_help, kind) && has_word(&value_help, kind))
            .map(String::from)
            .collect();
        let memory_help = help_section(help, "--load-mode");
        let load_modes_reported =
            successful && memory_help.contains("mmap+mlock") && has_word(&memory_help, "none");
        let host_help = help_section(help, "--host").to_ascii_lowercase();
        let unix_socket_reported =
            successful && host_help.contains("unix") && host_help.contains("socket");
        let fixed_values_reported = successful
            && has_word(&help_section(help, "--fit"), "off")
            && has_word(&help_section(help, "--device"), "none");
        Self {
            executable_path: artifact.path.clone(),
            executable_sha256: artifact.sha256.clone(),
            flags,
            flash_modes_reported,
            kv_types_reported,
            load_modes_reported,
            unix_socket_reported,
            fixed_values_reported,
            evidence: "selected_executable_help_only_not_inference",
        }
    }
    pub fn supports_flag(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }
    pub fn supports_flash_attention(&self) -> bool {
        self.supports_flag("--flash-attn") && self.flash_modes_reported
    }
    pub fn supports_kv_type(&self, kind: KvType) -> bool {
        self.supports_flag("--cache-type-k")
            && self.supports_flag("--cache-type-v")
            && self.kv_types_reported.contains(kind.arg())
    }
    pub fn supports_memory_settings(&self) -> bool {
        self.supports_flag("--load-mode") && self.load_modes_reported
            || ["--mmap", "--no-mmap", "--mlock", "--no-mlock"]
                .iter()
                .all(|f| self.supports_flag(f))
    }
    pub fn supports_unix_socket(&self) -> bool {
        self.unix_socket_reported
    }
    pub fn missing_core_flags(&self) -> Vec<&'static str> {
        [
            "--model",
            "--alias",
            "--host",
            "--port",
            "--parallel",
            "--offline",
            "--no-agent",
            "--no-webui",
            "--no-mmproj",
            "--fit",
            "--device",
            "--n-gpu-layers",
            "--ctx-size",
            "--threads",
            "--api-key",
        ]
        .into_iter()
        .filter(|flag| !self.supports_flag(flag))
        .collect()
    }
    /// A reported CLI candidate only. Exact socket ownership, readiness, model
    /// identity and protocol are separately validated when the runtime loads.
    pub fn protocol_candidate(&self) -> bool {
        self.missing_core_flags().is_empty()
            && self.unix_socket_reported
            && self.fixed_values_reported
    }
    pub(crate) fn arguments(
        &self,
        artifact: &Artifact,
        settings: &AdvancedSettings,
    ) -> Result<Vec<String>> {
        if self.executable_path != artifact.path || self.executable_sha256 != artifact.sha256 {
            return Err(Failure::ArtifactChanged);
        }
        if settings.prefill_threads.is_some_and(|n| n == 0 || n > 512)
            || settings.mmap.is_some() != settings.mlock.is_some()
            || settings.batch_size.is_some_and(|n| n == 0 || n > 32768)
            || settings
                .micro_batch_size
                .is_some_and(|n| n == 0 || n > 32768)
            || settings.micro_batch_size.is_some() && settings.batch_size.is_none()
            || settings
                .micro_batch_size
                .zip(settings.batch_size)
                .is_some_and(|(micro, batch)| micro > batch)
        {
            return Err(Failure::InvalidConfig);
        }
        // Quantized V requires flash attention in this reviewed adapter contract.
        if settings.cache_type_v.is_some_and(|v| v != KvType::F16)
            && settings.flash_attention != Some(FlashAttention::On)
        {
            return Err(Failure::InvalidConfig);
        }
        let mut args = Vec::new();
        let mut add = |flag: &str, value: String| -> Result<()> {
            if !self.flags.contains(flag) {
                return Err(Failure::UnsupportedCapability);
            }
            args.extend([flag.to_string(), value]);
            Ok(())
        };
        if let Some(threads) = settings.prefill_threads {
            add("--threads-batch", threads.to_string())?;
        }
        if let Some(batch) = settings.batch_size {
            add("--batch-size", batch.to_string())?;
        }
        if let Some(batch) = settings.micro_batch_size {
            add("--ubatch-size", batch.to_string())?;
        }
        if let Some(flash) = settings.flash_attention {
            if !self.flash_modes_reported {
                return Err(Failure::UnsupportedCapability);
            }
            add(
                "--flash-attn",
                match flash {
                    FlashAttention::On => "on",
                    FlashAttention::Off => "off",
                    FlashAttention::Auto => "auto",
                }
                .into(),
            )?;
        }
        for (flag, value) in [
            ("--cache-type-k", settings.cache_type_k),
            ("--cache-type-v", settings.cache_type_v),
        ] {
            if let Some(value) = value {
                if !self.kv_types_reported.contains(value.arg()) {
                    return Err(Failure::UnsupportedCapability);
                }
                add(flag, value.arg().into())?;
            }
        }
        if settings.mmap.is_some() || settings.mlock.is_some() {
            let mmap = settings.mmap.ok_or(Failure::InvalidConfig)?;
            let mlock = settings.mlock.ok_or(Failure::InvalidConfig)?;
            if self.flags.contains("--load-mode") && self.load_modes_reported {
                add(
                    "--load-mode",
                    match (mmap, mlock) {
                        (true, true) => "mmap+mlock",
                        (true, false) => "mmap",
                        (false, true) => "mlock",
                        (false, false) => "none",
                    }
                    .into(),
                )?;
            } else {
                let flag = if mmap { "--mmap" } else { "--no-mmap" };
                let lock_flag = if mlock { "--mlock" } else { "--no-mlock" };
                if !self.flags.contains(flag) || !self.flags.contains(lock_flag) {
                    return Err(Failure::UnsupportedCapability);
                }
                args.push(flag.into());
                args.push(lock_flag.into());
            }
        }
        Ok(args)
    }
}
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|s| s == word)
}
fn help_section(help: &str, flag: &str) -> String {
    let mut section = String::new();
    let mut found = false;
    for line in help.lines().take(4096) {
        if !found {
            if line.split_whitespace().any(|s| s.trim_matches(',') == flag) {
                found = true;
            } else {
                continue;
            }
        } else if line.trim_start().starts_with('-') && line.contains("--") {
            break;
        }
        section.push_str(line);
        section.push('\n');
        if section.len() >= 4096 {
            break;
        }
    }
    section
}

#[cfg(test)]
mod tests {
    use super::*;
    fn artifact() -> Artifact {
        Artifact {
            path: "/synthetic/runtime".into(),
            sha256: "a".repeat(64),
            source: "synthetic".into(),
            source_revision: "v1".into(),
        }
    }
    #[test]
    fn absent_flag_and_unrelated_help_values_cannot_enable_advanced_settings() {
        let a = artifact();
        let report = RuntimeCompatibility::observed(
            &a,
            "--threads N\nall settings on off auto f16 q4_0",
            true,
        );
        assert_eq!(
            report
                .arguments(
                    &a,
                    &AdvancedSettings {
                        batch_size: Some(128),
                        ..Default::default()
                    }
                )
                .unwrap_err(),
            Failure::UnsupportedCapability
        );
        let report =
            RuntimeCompatibility::observed(&a, "--flash-attn FLAG\n--other mode on off auto", true);
        assert_eq!(
            report
                .arguments(
                    &a,
                    &AdvancedSettings {
                        flash_attention: Some(FlashAttention::On),
                        ..Default::default()
                    }
                )
                .unwrap_err(),
            Failure::UnsupportedCapability
        );
    }
    #[test]
    fn legacy_memory_flags_are_exact_and_never_arbitrary() {
        let a = artifact();
        let report = RuntimeCompatibility::observed(&a, "--mmap\n--no-mmap\n--mlock", true);
        assert_eq!(
            report
                .arguments(
                    &a,
                    &AdvancedSettings {
                        mmap: Some(false),
                        mlock: Some(true),
                        ..Default::default()
                    }
                )
                .unwrap(),
            ["--no-mmap", "--mlock"]
        );
        assert_eq!(
            report
                .arguments(
                    &a,
                    &AdvancedSettings {
                        mmap: Some(true),
                        mlock: Some(false),
                        ..Default::default()
                    }
                )
                .unwrap_err(),
            Failure::UnsupportedCapability
        );
        let with_disable =
            RuntimeCompatibility::observed(&a, "--mmap\n--no-mmap\n--mlock\n--no-mlock", true);
        assert!(with_disable.supports_memory_settings());
        assert_eq!(
            with_disable
                .arguments(
                    &a,
                    &AdvancedSettings {
                        mmap: Some(true),
                        mlock: Some(false),
                        ..Default::default()
                    }
                )
                .unwrap(),
            ["--mmap", "--no-mlock"]
        );
        let report = RuntimeCompatibility::observed(&a, "--mmap\n--no-mmap\n--mlock", false);
        assert!(report
            .arguments(
                &a,
                &AdvancedSettings {
                    mmap: Some(true),
                    ..Default::default()
                }
            )
            .is_err());
    }
    #[test]
    fn batch_bounds_and_quantized_value_dependency_reject_before_launch() {
        let a = artifact();
        let report = RuntimeCompatibility::observed(&a, "--batch-size N\n--ubatch-size N", true);
        for settings in [
            AdvancedSettings {
                prefill_threads: Some(513),
                ..Default::default()
            },
            AdvancedSettings {
                batch_size: Some(0),
                ..Default::default()
            },
            AdvancedSettings {
                micro_batch_size: Some(32),
                ..Default::default()
            },
            AdvancedSettings {
                cache_type_v: Some(KvType::Q4_0),
                ..Default::default()
            },
        ] {
            assert_eq!(
                report.arguments(&a, &settings).unwrap_err(),
                Failure::InvalidConfig
            );
        }
    }
    #[test]
    fn protocol_candidate_requires_complete_fixed_flags_and_unix_description() {
        let a = artifact();
        let help="--model FILE\n--alias ID\n--host HOST UNIX socket paths\n--port N\n--parallel N\n--offline\n--no-agent\n--no-webui\n--no-mmproj\n--fit [on|off]\n--device NAME none\n--n-gpu-layers N\n--ctx-size N\n--threads N\n--api-key KEY";
        let c = RuntimeCompatibility::observed(&a, help, true);
        assert!(c.protocol_candidate());
        assert!(c.missing_core_flags().is_empty());
        for changed in [
            help.replace("--offline", "--not-offline"),
            help.replace("UNIX socket", "TCP address"),
            help.replace("[on|off]", "on"),
            help.replace("NAME none", "NAME"),
        ] {
            assert!(!RuntimeCompatibility::observed(&a, &changed, true).protocol_candidate());
        }
    }
}
