// Read-only native inventory. No model, kernel, download, driver installation,
// private profile, keychain, screen capture, or application automation is used.
import Foundation
import Metal

struct MetalDevice: Encodable {
    let ordinal: Int
    let name: String
    let unified_memory: Bool
    let low_power: Bool
    let removable: Bool
    let recommended_working_set_bytes: UInt64
    let maximum_single_buffer_bytes: Int
    let apple_gpu_family: Bool
    let mac_gpu_family_2: Bool
}

struct Inventory: Encodable {
    let schema_version = 1
    let evidence = "native_os_inventory_only"
    let os = "macos"
    let architecture: String
    let os_version: String
    let logical_cpu_count: Int
    let active_cpu_count: Int
    let physical_memory_bytes: UInt64
    let devices: [MetalDevice]
    let backend_runtime_tested = false
    let model_placement_observed = false
    let note = "Metal enumeration is not MLX installation, model compatibility, free memory, or performance evidence. Device ordinals are valid only for this observation."
}

#if arch(arm64)
let architecture = "aarch64"
#elseif arch(x86_64)
let architecture = "x86_64"
#else
let architecture = "unknown"
#endif
let info = ProcessInfo.processInfo
let version = info.operatingSystemVersion
let inventory = Inventory(
    architecture: architecture,
    os_version: "\(version.majorVersion).\(version.minorVersion).\(version.patchVersion)",
    logical_cpu_count: info.processorCount,
    active_cpu_count: info.activeProcessorCount,
    physical_memory_bytes: info.physicalMemory,
    devices: MTLCopyAllDevices().enumerated().map { index, device in
        MetalDevice(
            ordinal: index,
            name: device.name,
            unified_memory: device.hasUnifiedMemory,
            low_power: device.isLowPower,
            removable: device.isRemovable,
            recommended_working_set_bytes: device.recommendedMaxWorkingSetSize,
            maximum_single_buffer_bytes: device.maxBufferLength,
            apple_gpu_family: device.supportsFamily(.apple1),
            mac_gpu_family_2: device.supportsFamily(.mac2)
        )
    }
)
let encoder = JSONEncoder()
encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
let bytes = try encoder.encode(inventory)
FileHandle.standardOutput.write(bytes)
FileHandle.standardOutput.write(Data([10]))
