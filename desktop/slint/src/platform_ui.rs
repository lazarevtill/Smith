//! Target-specific UI policy, not hardware qualification. Runtime probes remain
//! authoritative. Test every target as data without pretending to execute it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Platform {
    pub os: &'static str,
    pub arch: &'static str,
}
#[derive(Clone, Copy, Debug)]
pub struct Choice {
    pub id: Option<&'static str>,
    pub label: &'static str,
}
const fn choice(id: &'static str, label: &'static str) -> Choice {
    Choice {
        id: Some(id),
        label,
    }
}
impl Platform {
    pub const fn current() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        }
    }
    pub fn mlx(self) -> bool {
        self.os == "macos" && self.arch == "aarch64"
    }
    pub fn owned_runtime(self) -> bool {
        matches!(self.os, "macos" | "linux")
    }
    pub fn pickers(self) -> bool {
        self.os == "macos"
    }
    pub fn providers(self) -> Vec<Choice> {
        let mut choices = vec![
            choice("mock", "Demo"),
            choice("ollama", "Ollama"),
            choice("openai", "OpenAI-compatible"),
        ];
        if self.owned_runtime() {
            choices.push(choice("managed", "Owned GGUF model"));
        }
        if self.mlx() {
            choices.push(choice("mlx", "Owned MLX model"));
        }
        choices
    }
    pub fn backends(self) -> Vec<Choice> {
        let mut choices = vec![
            Choice {
                id: None,
                label: "Automatic backend",
            },
            choice("llama_cpu", "CPU"),
        ];
        match self.os {
            "macos" => choices.push(choice("llama_metal", "Metal")),
            "windows" | "linux" => choices.extend([
                choice("llama_cuda", "CUDA"),
                choice("llama_hip", "ROCm / HIP"),
                choice("llama_sycl", "Intel SYCL"),
                choice("llama_vulkan", "Vulkan"),
            ]),
            _ => {}
        }
        choices
    }
    pub fn engines(self) -> Vec<&'static str> {
        if self.mlx() {
            vec!["GGUF · local runtime", "Apple MLX · native Metal"]
        } else {
            vec!["GGUF · local runtime"]
        }
    }
}
/// Unknown or foreign saved IDs stay unselected; they must not silently become
/// Auto/CPU/another provider when a filtered menu changes position.
pub fn selected(choices: &[Choice], id: Option<&str>) -> i32 {
    choices
        .iter()
        .position(|c| c.id == id)
        .map_or(-1, |i| i as i32)
}
pub fn resolve(choices: &[Choice], index: i32) -> Result<Option<&'static str>, String> {
    usize::try_from(index)
        .ok()
        .and_then(|i| choices.get(i))
        .map(|c| c.id)
        .ok_or_else(|| {
            "Choose an available option for this platform. Saved settings have not been changed."
                .into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn platform(os: &'static str, arch: &'static str) -> Platform {
        Platform { os, arch }
    }
    #[test]
    fn apple_silicon_and_intel_mac_have_different_mlx_availability() {
        for arch in ["aarch64", "x86_64"] {
            let p = platform("macos", arch);
            assert_eq!(p.mlx(), arch == "aarch64");
            assert_eq!(p.engines().len(), if p.mlx() { 2 } else { 1 });
            assert!(p.owned_runtime() && p.pickers());
            let ids: Vec<_> = p.backends().iter().map(|c| c.id).collect();
            assert_eq!(ids, vec![None, Some("llama_cpu"), Some("llama_metal")]);
            assert_eq!(selected(&p.providers(), Some("mlx")) >= 0, p.mlx());
        }
    }
    #[test]
    fn linux_and_windows_do_not_offer_apple_runtime_or_unimplemented_pickers() {
        for os in ["linux", "windows"] {
            for arch in ["x86_64", "aarch64"] {
                let p = platform(os, arch);
                assert!(!p.mlx() && !p.pickers());
                assert_eq!(p.owned_runtime(), os == "linux");
                assert_eq!(p.engines(), vec!["GGUF · local runtime"]);
                assert_eq!(
                    p.backends().iter().map(|c| c.id).collect::<Vec<_>>(),
                    vec![
                        None,
                        Some("llama_cpu"),
                        Some("llama_cuda"),
                        Some("llama_hip"),
                        Some("llama_sycl"),
                        Some("llama_vulkan")
                    ]
                );
                assert_eq!(
                    selected(&p.providers(), Some("managed")) >= 0,
                    os == "linux"
                );
                assert_eq!(selected(&p.providers(), Some("mlx")), -1);
            }
        }
    }
    #[test]
    fn all_filtered_positions_round_trip_exact_ids_and_reject_bad_indices() {
        for os in ["macos", "linux", "windows", "unknown"] {
            for arch in ["aarch64", "x86_64"] {
                let p = platform(os, arch);
                for choices in [p.backends(), p.providers()] {
                    for (i, c) in choices.iter().enumerate() {
                        assert_eq!(selected(&choices, c.id), i as i32);
                        assert_eq!(resolve(&choices, i as i32).unwrap(), c.id);
                    }
                    for i in [-1, i32::MAX, choices.len() as i32] {
                        assert!(resolve(&choices, i).is_err());
                    }
                    assert_eq!(selected(&choices, Some("unknown_saved_value")), -1);
                }
            }
        }
    }
    #[test]
    fn foreign_saved_backend_cannot_be_silently_reinterpreted_or_reset() {
        let mac = platform("macos", "aarch64").backends();
        let linux = platform("linux", "x86_64").backends();
        assert_eq!(selected(&mac, Some("llama_cuda")), -1);
        assert!(resolve(&mac, selected(&mac, Some("llama_cuda"))).is_err());
        assert!(resolve(&linux, selected(&linux, Some("llama_metal"))).is_err());
        assert_eq!(resolve(&mac, 2).unwrap(), Some("llama_metal"));
        assert_eq!(resolve(&linux, 2).unwrap(), Some("llama_cuda"));
        assert_eq!(resolve(&mac, 0).unwrap(), None);
    }
    #[test]
    fn unknown_platform_fails_closed() {
        let p = platform("unknown", "unknown");
        assert!(!p.mlx() && !p.owned_runtime() && !p.pickers());
        assert_eq!(p.backends().len(), 2);
        assert_eq!(p.providers().len(), 3);
    }
}
