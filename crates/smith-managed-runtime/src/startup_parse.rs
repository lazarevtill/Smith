//! Narrow grammar for llama.cpp's load_tensors diagnostics. This interprets
//! statements from the selected process; it does not verify device kernels.
//! Source grammar and limitations are recorded in docs/NATIVE-RUNTIME.md.
use crate::startup::{
    ReportedBufferKind, ReportedModelBuffer, ReportedOffload, StartupPlacement,
    StartupUnknownReason,
};

pub(crate) fn parse<'a>(
    lines: impl Iterator<Item = &'a str>,
    incomplete: bool,
) -> StartupPlacement {
    use StartupUnknownReason as Reason;
    if incomplete {
        return unknown(Reason::IncompleteOutput);
    }
    let mut offload = None;
    let mut buffers = Vec::new();
    for line in lines {
        let Some(body) = line.strip_prefix("load_tensors: ") else {
            continue;
        };
        let body = body.trim_start_matches(' ');
        if let Some(value) = body.strip_prefix("offloaded ") {
            let Some(value) = value.strip_suffix(" layers to GPU") else {
                return unknown(Reason::ConflictingOrMalformedRecords);
            };
            let Some((loaded, total)) = value.split_once('/') else {
                return unknown(Reason::ConflictingOrMalformedRecords);
            };
            let (Some(loaded), Some(total)) = (integer(loaded), integer(total)) else {
                return unknown(Reason::ConflictingOrMalformedRecords);
            };
            if total == 0 || total > 100_000 || loaded > total || offload.is_some() {
                return unknown(Reason::ConflictingOrMalformedRecords);
            }
            offload = Some(ReportedOffload {
                offloaded_layers: loaded,
                total_layers: total,
            });
        } else if body.contains("model buffer size") {
            let Some((label, value)) = body.split_once(" model buffer size = ") else {
                return unknown(Reason::ConflictingOrMalformedRecords);
            };
            let Some((kind, index)) = buffer_kind(label) else {
                return unknown(Reason::UnsupportedBufferLabel);
            };
            let Some(size) = value
                .trim_start_matches(' ')
                .strip_suffix(" MiB")
                .and_then(hundredths)
            else {
                return unknown(Reason::ConflictingOrMalformedRecords);
            };
            if buffers.len() >= 16 {
                return unknown(Reason::ConflictingOrMalformedRecords);
            }
            buffers.push(ReportedModelBuffer {
                kind,
                runtime_device_index: index,
                size_mib_hundredths: size,
            });
        }
    }
    let Some(offload) = offload else {
        return unknown(Reason::NoSupportedRecords);
    };
    if buffers.is_empty() || !buffers.iter().any(|v| v.size_mib_hundredths > 0) {
        return unknown(Reason::NoSupportedRecords);
    }
    // Positive GPU offload must have a positive recognized GPU buffer. Never
    // infer Metal from a request, enumeration, or CPU-only buffer record.
    if offload.offloaded_layers > 0
        && !buffers.iter().any(|v| {
            matches!(
                v.kind,
                ReportedBufferKind::MetalShared
                    | ReportedBufferKind::MetalMapped
                    | ReportedBufferKind::MetalPrivate
            ) && v.size_mib_hundredths > 0
        })
    {
        return unknown(Reason::ConflictingOrMalformedRecords);
    }
    StartupPlacement::RuntimeReported {
        format: "llama_cpp_load_tensors_v1",
        offload,
        model_buffers: buffers,
    }
}
fn unknown(reason: StartupUnknownReason) -> StartupPlacement {
    StartupPlacement::Unknown { reason }
}
fn integer(value: &str) -> Option<u32> {
    if value.is_empty() || value.len() > 6 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}
fn hundredths(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.')?;
    if whole.is_empty()
        || whole.len() > 10
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() != 2
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // Preserve the log's rounded hundredths of MiB, not imaginary exact bytes.
    whole
        .parse::<u64>()
        .ok()?
        .checked_mul(100)?
        .checked_add(fraction.parse().ok()?)
}
fn buffer_kind(label: &str) -> Option<(ReportedBufferKind, Option<u16>)> {
    // Only source-reviewed CPU/Metal labels are interpreted in this increment.
    // The numeric index is runtime-local; it is not an OS inventory identity.
    if label == "CPU" {
        return Some((ReportedBufferKind::Cpu, None));
    }
    if label == "CPU_Mapped" {
        return Some((ReportedBufferKind::CpuMapped, None));
    }
    let value = label.strip_prefix("MTL")?;
    let (digits, kind) = if let Some(v) = value.strip_suffix("_Mapped") {
        (v, ReportedBufferKind::MetalMapped)
    } else if let Some(v) = value.strip_suffix("_Private") {
        (v, ReportedBufferKind::MetalPrivate)
    } else {
        (value, ReportedBufferKind::MetalShared)
    };
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let index: u16 = digits.parse().ok()?;
    // The reviewed Metal implementation has a sixteen-device maximum.
    (index < 16).then_some((kind, Some(index)))
}

#[cfg(test)]
mod tests {
    use super::*;
    const OFFLOAD: &str = "load_tensors: offloaded 40/41 layers to GPU";
    const GPU: &str = "load_tensors:  MTL0_Mapped model buffer size =  5099.18 MiB";
    const CPU: &str = "load_tensors:   CPU_Mapped model buffer size =    27.04 MiB";
    #[test]
    fn source_grammar_preserves_reported_counts_and_rounded_units_only() {
        let value = parse([OFFLOAD, GPU, CPU].into_iter(), false);
        let StartupPlacement::RuntimeReported {
            offload,
            model_buffers,
            ..
        } = value
        else {
            panic!("expected report")
        };
        assert_eq!(offload.offloaded_layers, 40);
        assert_eq!(offload.total_layers, 41);
        assert_eq!(model_buffers.len(), 2);
        assert_eq!(model_buffers[0].kind, ReportedBufferKind::MetalMapped);
        assert_eq!(model_buffers[0].runtime_device_index, Some(0));
        assert_eq!(model_buffers[0].size_mib_hundredths, 509_918);
        assert_eq!(model_buffers[1].kind, ReportedBufferKind::CpuMapped);
        assert!(matches!(
            parse(
                ["load_tensors: offloaded 0/41 layers to GPU", CPU].into_iter(),
                false
            ),
            StartupPlacement::RuntimeReported { .. }
        ));
    }
    #[test]
    fn missing_incomplete_ambiguous_unknown_and_malformed_stay_unknown() {
        for values in [
            vec![GPU],
            vec![OFFLOAD],
            vec![OFFLOAD, CPU],
            vec![OFFLOAD, GPU, OFFLOAD],
            vec!["load_tensors: offloaded 42/41 layers to GPU", GPU],
            vec!["load_tensors: offloaded 1/0 layers to GPU", GPU],
            vec!["load_tensors: offloaded +1/41 layers to GPU", GPU],
            vec![
                OFFLOAD,
                "load_tensors: CUDA0 model buffer size =  10.00 MiB",
            ],
            vec![OFFLOAD, "load_tensors: MTL0 model buffer size = NaN MiB"],
            vec![OFFLOAD, "load_tensors: MTL0 model buffer size = -2.00 MiB"],
            vec![
                OFFLOAD,
                "load_tensors: MTL0 model buffer size = 999999999999999999999999999.00 MiB",
            ],
            vec![
                OFFLOAD,
                "load_tensors: MTL16 model buffer size =  10.00 MiB",
            ],
            vec![
                OFFLOAD,
                "load_tensors: MTL0/file model buffer size =  10.00 MiB",
            ],
            vec![
                OFFLOAD,
                "load_tensors: MTL0 model buffer size = 10.00 MiB extra",
            ],
            vec![
                "requested --device MTL0 --n-gpu-layers 40",
                "MTL0: Apple M1 (10922 MiB, 10922 MiB free)",
            ],
        ] {
            assert!(matches!(
                parse(values.into_iter(), false),
                StartupPlacement::Unknown { .. }
            ));
        }
        assert!(matches!(
            parse([OFFLOAD, GPU].into_iter(), true),
            StartupPlacement::Unknown {
                reason: StartupUnknownReason::IncompleteOutput
            }
        ));
        let many = std::iter::once(OFFLOAD).chain(std::iter::repeat_n(GPU, 17));
        assert!(matches!(
            parse(many, false),
            StartupPlacement::Unknown { .. }
        ));
    }
}
